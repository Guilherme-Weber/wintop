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
    /// Clock médio (MHz).
    pub freq_mhz: Metric<f64>,
    /// Clock efetivo de cada lógico (MHz), na ordem de `per_logical`.
    pub freq_per_logical: Metric<Vec<f64>>,
    pub counts: Metric<Counts>,
    pub uptime: Duration,
}

/// Compressão de memória do Windows (`SystemStoreInformation`, subclasse 22):
/// bytes físicos em RAM ocupados pelo store e estimativa do tamanho original antes da compressão.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CompressedMemory {
    /// Bytes físicos em RAM ocupados pela memória comprimida (`WorkingSetSize`).
    pub compressed: u64,
    /// Estimativa do tamanho original (descomprimido) dos dados comprimidos em RAM.
    pub uncompressed: u64,
}

/// Hardware de RAM lido do SMBIOS (Type 17, estático por boot).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryHardware {
    /// Geração (ex.: "DDR4", "DDR5", "LPDDR5").
    pub kind: &'static str,
    /// Velocidade configurada (ou máxima, se a configurada não for informada), em MT/s.
    pub speed_mt: Option<u32>,
    /// Slots ocupados e total de slots na placa-mãe.
    pub slots_used: u32,
    pub slots_total: u32,
    /// Formato (ex.: "DIMM", "SODIMM").
    pub form_factor: Option<&'static str>,
}

#[derive(Clone, Debug)]
pub struct Memory {
    pub total: Metric<u64>,
    pub used: Metric<u64>,
    pub commit: Metric<(u64, u64)>,
    /// Pico de memória confirmada desde o boot (`GetPerformanceInfo.CommitPeak`), bytes.
    pub commit_peak: Metric<u64>,
    /// Memória comprimida em RAM e tamanho original (descomprimido) estimado.
    pub compressed: Metric<CompressedMemory>,
    /// Standby + Modified (igual ao "Em cache" do Gerenciador de Tarefas).
    pub cached: Metric<u64>,
    /// As duas partes do "em cache": standby (livre para reuso) e modificada (a gravar).
    pub standby: Metric<u64>,
    pub modified: Metric<u64>,
    pub paged_pool: Metric<u64>,
    pub nonpaged_pool: Metric<u64>,
    /// Arquivo de paginação (usado e total, em bytes).
    pub pagefile_usage: Metric<(u64, u64)>,
    /// Paginação real em disco: `(páginas lidas/s, páginas gravadas/s)`.
    pub paging: Metric<(f64, f64)>,
    /// Hardware de RAM (SMBIOS Type 17, estático por boot).
    pub hardware: Metric<MemoryHardware>,
    /// RAM reservada pelo hardware (`GetPhysicallyInstalledSystemMemory - total`, bytes).
    pub hardware_reserved: Metric<u64>,
    /// Páginas defeituosas marcadas pelo kernel (`SystemMemoryListInformation.BadPageCount`).
    pub bad_pages: Metric<u64>,
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
    /// Transferências por segundo (leituras + gravações) e pedidos na fila agora.
    pub iops: Metric<f64>,
    pub queue: Metric<u32>,
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
    /// Pacotes com erro desde que a interface subiu: (entrada, saída).
    pub errors: Metric<(u64, u64)>,
}

#[derive(Clone, Debug)]
pub struct Fan {
    pub percent: Metric<u32>,
    pub rpm: Metric<u32>,
}

#[derive(Clone, Debug)]
pub struct Gpu {
    /// Posição na lista do wintop (as GPUs do NVML, na ordem dele, e depois as só do DXGI).
    pub index: u32,
    /// GPU sem NVML (AMD, Intel, ou NVIDIA com o NVML em falha): o motivo, que vale para todos
    /// os campos só do NVML (a interface mostra uma linha em vez de repetir em cada um). Uso,
    /// encode, decode e VRAM vêm do DXGI e do PDH.
    pub no_nvml: Option<Missing>,
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
    /// Engine mais usada fora o 3D e as de vídeo (PDH do adaptador): tipo ("Copy",
    /// "Compute_0"...) e uso, 0-100.
    pub other_engine: Metric<(String, u32)>,
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
    /// Memória compartilhada da GPU por categoria (`\GPU Process Memory(*)\Shared Usage`).
    pub shared: Metric<VramBreakdown>,
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

/// Sensores da CPU pelo PawnIO (AMD: `AMDFamily17`; Intel: `IntelMSR`).
#[derive(Clone, Debug)]
pub struct CpuSensors {
    /// Nome da leitura principal: "Tctl" (AMD) ou "Package" (Intel).
    pub label: &'static str,
    /// Nome das zonas quando falham: "CCD" (AMD) ou "Core" (Intel).
    pub zone: &'static str,
    /// Limite térmico para as cores e o termômetro (95 °C na AMD; TjMax na Intel).
    pub limit: f64,
    /// Leitura principal em °C (Tctl na AMD; maior pacote na Intel).
    pub tctl: Metric<f64>,
    /// (nome, °C) por zona: "CCD1".. na AMD, "Core 0".. (e "Package n" com vários soquetes) na
    /// Intel.
    pub ccds: Metric<Vec<(String, f64)>>,
    /// Watts.
    pub package_power: Metric<f64>,
}

impl CpuSensors {
    /// Sensores da AMD (Tctl e CCDs; limite de 95 °C).
    pub fn amd(
        tctl: Metric<f64>,
        ccds: Metric<Vec<(String, f64)>>,
        package_power: Metric<f64>,
    ) -> Self {
        Self {
            label: "Tctl",
            zone: "CCD",
            limit: 95.0,
            tctl,
            ccds,
            package_power,
        }
    }
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
    /// Previsão de falha do SMART: `Ok(true)` = o disco prevê falha próxima.
    pub health: Metric<bool>,
}

/// De onde vêm os frames do jogo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameSource {
    /// ETW: DXGI `Present_Start` do jogo (exato; D3D10/11/12).
    Etw,
    /// Pipe de frametime do RTSS (exato; qualquer API, só para jogos abertos depois do wintop).
    Pipe,
    /// Amostras da Shared Memory do RTSS (aproximado).
    Approx,
}

/// Frame generation (DLSS FG) detectado pelos marcadores Reflex.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameGen {
    /// Frames reais (simulados pelo jogo) no último segundo.
    pub real_fps: f64,
    /// Frames apresentados por frame real (2x, 3x, 4x), quando o jogo informa.
    pub multiplier: Option<u32>,
    /// Razão efetiva (apresentados / reais) no último segundo (MFG dinâmico).
    pub effective: Option<f64>,
}

impl FrameGen {
    /// Texto do multiplicador ("4x", ou razão efetiva como "2.5x" no MFG dinâmico).
    pub fn ratio_text(&self) -> Option<String> {
        if let Some(r) = self.effective.filter(|r| r.is_finite() && *r >= 1.5) {
            if let Some(m) = self.multiplier
                && (r - f64::from(m)).abs() < 0.2
            {
                return Some(format!("{m}x"));
            }
            if (r - r.round()).abs() < 0.15 {
                return Some(format!("{}x", r.round() as u32));
            }
            return Some(format!("{}x", crate::fmt::num(r, 1)));
        }
        self.multiplier.map(|m| format!("{m}x"))
    }
}

/// Jogo acompanhado pelo RTSS.
#[derive(Clone, Debug, PartialEq)]
pub struct Game {
    pub pid: u32,
    pub name: String,
    pub api: &'static str,
    pub resolution: Metric<(u32, u32)>,
    pub source: FrameSource,
    /// FPS apresentado (com frame gen, inclui os frames gerados).
    pub fps: Metric<f64>,
    /// Frame gen ativo: FPS real e multiplicador. `None`: sem frame gen (ou jogo sem Reflex).
    pub framegen: Option<FrameGen>,
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

impl Game {
    /// Valores da Shared Memory (jogo aberto antes do wintop, sem ETW): aproximados.
    #[cfg(test)]
    pub fn approx(&self) -> bool {
        self.source == FrameSource::Approx
    }
}

/// Estado do RTSS e do jogo (thread do RTSS, a cada 250 ms).
#[derive(Clone, Debug, PartialEq)]
pub struct Fps {
    /// RTSS rodando (versão da Shared Memory).
    pub rtss: Metric<String>,
    /// Pipe de frametime: quantas instâncias e clientes conectados.
    pub pipe: Metric<(usize, usize)>,
    /// Sessão ETW dos frames (DXGI + Reflex).
    pub etw: Metric<&'static str>,
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
    /// Working set privado (bytes) do jogo em foco (`(pid, bytes)`), via `SystemProcessInformation`.
    pub game_ram: Option<(u32, u64)>,
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
            game_ram: None,
            took: Duration::ZERO,
        }
    }
}

fn any_fail(visit: impl FnOnce(&mut dyn FnMut(&str, Option<&Missing>))) -> bool {
    let mut hit = false;
    visit(&mut |_, e| {
        if matches!(e, Some(Missing::Fail(_))) {
            hit = true;
        }
    });
    hit
}

impl Cpu {
    /// Percorre cada métrica deste bloco como `(nome, Option<&Missing>)` (`None` = `Ok`).
    /// Compartilhado entre o rodapé (`failing_sources`) e o log de debug (`debug.rs`) para que
    /// um campo novo nunca fique de fora de um dos dois.
    pub fn visit_metrics(&self, mut f: impl FnMut(&str, Option<&Missing>)) {
        f("name", self.name.as_ref().err());
        f("topology", self.topology.as_ref().err());
        f("base_mhz", self.base_mhz.as_ref().err());
        f("virtualization", self.virtualization.as_ref().err());
        f("usage", self.usage.as_ref().err());
        f("per_logical", self.per_logical.as_ref().err());
        f("kernel_per_logical", self.kernel_per_logical.as_ref().err());
        f("kernel", self.kernel.as_ref().err());
        f("freq_mhz", self.freq_mhz.as_ref().err());
        f("freq_per_logical", self.freq_per_logical.as_ref().err());
        f("counts", self.counts.as_ref().err());
    }

    pub fn has_failure(&self) -> bool {
        any_fail(|f| self.visit_metrics(f))
    }
}

impl CpuSensors {
    pub fn visit_metrics(&self, mut f: impl FnMut(&str, Option<&Missing>)) {
        f("tctl", self.tctl.as_ref().err());
        f("ccds", self.ccds.as_ref().err());
        f("package_power", self.package_power.as_ref().err());
    }

    pub fn has_failure(&self) -> bool {
        any_fail(|f| self.visit_metrics(f))
    }
}

impl Memory {
    pub fn visit_metrics(&self, mut f: impl FnMut(&str, Option<&Missing>)) {
        f("total", self.total.as_ref().err());
        f("used", self.used.as_ref().err());
        f("commit", self.commit.as_ref().err());
        f("commit_peak", self.commit_peak.as_ref().err());
        f("compressed", self.compressed.as_ref().err());
        f("cached", self.cached.as_ref().err());
        f("standby", self.standby.as_ref().err());
        f("modified", self.modified.as_ref().err());
        f("paged_pool", self.paged_pool.as_ref().err());
        f("nonpaged_pool", self.nonpaged_pool.as_ref().err());
        f("pagefile_usage", self.pagefile_usage.as_ref().err());
        f("paging", self.paging.as_ref().err());
        f("hardware", self.hardware.as_ref().err());
        f("hardware_reserved", self.hardware_reserved.as_ref().err());
        f("bad_pages", self.bad_pages.as_ref().err());
    }

    pub fn has_failure(&self) -> bool {
        any_fail(|f| self.visit_metrics(f))
    }
}

impl Disk {
    pub fn visit_metrics(&self, mut f: impl FnMut(&str, Option<&Missing>)) {
        f("model", self.model.as_ref().err());
        f("bus", self.bus.as_ref().err());
        f("ssd", self.ssd.as_ref().err());
        f("capacity", self.capacity.as_ref().err());
        f("pool_member", self.pool_member.as_ref().err());
        f("active", self.active.as_ref().err());
        f("read", self.read.as_ref().err());
        f("write", self.write.as_ref().err());
        f("response", self.response.as_ref().err());
        f("iops", self.iops.as_ref().err());
        f("queue", self.queue.as_ref().err());
    }

    pub fn has_failure(&self) -> bool {
        any_fail(|f| self.visit_metrics(f))
    }
}

impl Nic {
    pub fn visit_metrics(&self, mut f: impl FnMut(&str, Option<&Missing>)) {
        f("link", self.link.as_ref().err());
        f("send", self.send.as_ref().err());
        f("recv", self.recv.as_ref().err());
        f("errors", self.errors.as_ref().err());
    }

    pub fn has_failure(&self) -> bool {
        any_fail(|f| self.visit_metrics(f))
    }
}

impl Gpu {
    pub fn visit_metrics(&self, mut f: impl FnMut(&str, Option<&Missing>)) {
        f("name", self.name.as_ref().err());
        f("driver", self.driver.as_ref().err());
        f("usage", self.usage.as_ref().err());
        f("dedicated", self.dedicated.as_ref().err());
        f("shared", self.shared.as_ref().err());
        f("encoder", self.encoder.as_ref().err());
        f("decoder", self.decoder.as_ref().err());
        f("other_engine", self.other_engine.as_ref().err());
        if self.no_nvml.is_none() {
            f("vbios", self.vbios.as_ref().err());
            f("mem_activity", self.mem_activity.as_ref().err());
            f("physical", self.physical.as_ref().err());
            f("temp", self.temp.as_ref().err());
            f("temp_slowdown", self.temp_slowdown.as_ref().err());
            f("clock_graphics", self.clock_graphics.as_ref().err());
            f("clock_sm", self.clock_sm.as_ref().err());
            f("clock_mem", self.clock_mem.as_ref().err());
            f("clock_video", self.clock_video.as_ref().err());
            f("power", self.power.as_ref().err());
            f("fans", self.fans.as_ref().err());
            if let Ok(fans) = &self.fans {
                for (i, fan) in fans.iter().enumerate() {
                    f(&format!("fan{i}.percent"), fan.percent.as_ref().err());
                    f(&format!("fan{i}.rpm"), fan.rpm.as_ref().err());
                }
            }
            f("pcie_link", self.pcie_link.as_ref().err());
            f("pstate", self.pstate.as_ref().err());
            f("throttle", self.throttle.as_ref().err());
        } else {
            f("nvml", self.no_nvml.as_ref());
        }
    }

    pub fn has_failure(&self) -> bool {
        any_fail(|f| self.visit_metrics(f))
    }
}

impl OwnUsage {
    pub fn visit_metrics(&self, mut f: impl FnMut(&str, Option<&Missing>)) {
        f("cpu", self.cpu.as_ref().err());
        f("private_ws", self.private_ws.as_ref().err());
        f("commit", self.commit.as_ref().err());
    }

    pub fn has_failure(&self) -> bool {
        any_fail(|f| self.visit_metrics(f))
    }
}

impl Game {
    pub fn visit_metrics(&self, mut f: impl FnMut(&str, Option<&Missing>)) {
        f("resolution", self.resolution.as_ref().err());
        f("fps", self.fps.as_ref().err());
        f("frametime", self.frametime.as_ref().err());
        f("lows", self.lows.as_ref().err());
        f("gpu_time", self.gpu_time.as_ref().err());
        f("bound", self.bound.as_ref().err());
    }

    pub fn has_failure(&self) -> bool {
        any_fail(|f| self.visit_metrics(f))
    }
}

impl GpuSlow {
    pub fn visit_metrics(&self, mut f: impl FnMut(&str, Option<&Missing>)) {
        f("pcie_tx", self.pcie_tx.as_ref().err());
        f("pcie_rx", self.pcie_rx.as_ref().err());
        f("vram", self.vram.as_ref().err());
        f("shared", self.shared.as_ref().err());
        if let Some(procs) = &self.procs {
            f("procs", procs.as_ref().err());
            if let Ok(pl) = procs {
                let eng = pl.iter().find_map(|x| {
                    x.engines
                        .as_ref()
                        .err()
                        .filter(|e| matches!(e, Missing::Fail(_)))
                });
                f("engines", eng);
            }
        }
    }
}

impl DiskTemp {
    pub fn visit_metrics(&self, mut f: impl FnMut(&str, Option<&Missing>)) {
        f("temp", self.celsius.as_ref().err());
        f("health", self.health.as_ref().err());
    }
}
