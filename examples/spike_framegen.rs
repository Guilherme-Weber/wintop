//! Spike: frame generation (DLSS FG) por ETW e pelos marcadores Reflex do RTSS.
//!
//! Pergunta: dá para separar os frames **reais** dos **gerados** sem o Afterburner, e quanto
//! custa? Com o jogo rodando (DLSS FG + Reflex ligados), em duas fases:
//!
//! 1. **Descoberta** (`DESCOBERTA` s): liga, numa sessão ETW em tempo real, todos os providers
//!    registrados no processo do jogo (`EnumerateTraceGuidsEx`) e conta os eventos do PID por
//!    (provider, evento), decodificados pelo TDH. Mostra o que existe de verdade (o provider de
//!    PC Latency/Reflex da NVIDIA, Streamline, eventos com "Frame"/"Marker" nos campos).
//! 2. **Medição** (segundo a segundo, até Ctrl+C ou o limite): DXGI `Present_Start` (todos os
//!    Present do jogo), DxgKrnl por nome (flips/PresentHistory), marcadores Reflex por ETW (os
//!    providers da descoberta com "PCL"/"Reflex"/"Latency" no nome, campo `Marker`) e, pela
//!    Shared Memory do RTSS (v2.21), FPS e quantas vezes `qwSimStartTime` e
//!    `qwPresentStartTime` mudaram (lidos a cada 1 ms). CPU do consumidor ETW e da leitura do
//!    RTSS medidas separadas (`GetThreadTimes`).
//!
//! Tudo vai também para `spike_framegen_<hora>.txt` na pasta atual. Precisa de terminal elevado
//! (sessão ETW). `cargo run --release --example spike_framegen [segundos] [pid]`

#[path = "common/mod.rs"]
mod common;

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::io::Write as _;
use std::os::windows::io::AsRawHandle;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, FILETIME, HANDLE, WIN32_ERROR,
};
use windows::Win32::System::Console::SetConsoleCtrlHandler;
use windows::Win32::System::Diagnostics::Etw::{
    CONTROLTRACE_HANDLE, CloseTrace, ControlTraceW, ENABLE_TRACE_PARAMETERS,
    ENABLE_TRACE_PARAMETERS_VERSION_2, EVENT_CONTROL_CODE_ENABLE_PROVIDER, EVENT_FILTER_DESCRIPTOR,
    EVENT_FILTER_TYPE_PID, EVENT_RECORD, EVENT_TRACE_CONTROL_STOP, EVENT_TRACE_LOGFILEW,
    EVENT_TRACE_PROPERTIES, EVENT_TRACE_REAL_TIME_MODE, EnableTraceEx2, EnumerateTraceGuidsEx,
    OpenTraceW, PROCESS_TRACE_MODE_EVENT_RECORD, PROCESS_TRACE_MODE_REAL_TIME,
    PROPERTY_DATA_DESCRIPTOR, ProcessTrace, StartTraceW, TRACE_EVENT_INFO, TdhGetEventInformation,
    TdhGetProperty, TdhGetPropertySize, TraceGuidQueryInfo, TraceGuidQueryList,
    WNODE_FLAG_TRACED_GUID,
};
use windows::Win32::System::Threading::GetThreadTimes;
use windows::Win32::System::Threading::{GetCurrentProcess, GetCurrentThread, GetProcessTimes};
use windows::core::{GUID, PCWSTR, PWSTR};

const SESSION: &str = "wintop-spike-framegen";
const DESCOBERTA: u64 = 10;

const DXGI: GUID = GUID::from_u128(0xCA11C036_0102_4A2D_A6AD_F03CFED5D3C9);
const DXGKRNL: GUID = GUID::from_u128(0x802EC45A_1E99_4B83_9920_87C98277BA9D);
/// DXGI `Present_Start`.
const DXGI_PRESENT_START: u16 = 42;
/// DxgKrnl: Base | Present (keywords do manifesto; o spike conta por nome o que vier).
const DXGKRNL_KEYWORDS: u64 = 0x1 | 0x800_0000;
/// Canal dos eventos TraceLogging (metadados no próprio evento: sem cache por id).
const CHANNEL_TRACELOGGING: u8 = 11;

// RTSS_SHARED_MEMORY (offsets do MSVC, header v2.21; ver spike_rtss)
const RTSS_SIGNATURE: u32 = 0x5254_5353;
const OFF_VERSION: usize = 4;
const OFF_APP_ENTRY_SIZE: usize = 8;
const OFF_APP_ARR_OFFSET: usize = 12;
const OFF_APP_ARR_SIZE: usize = 16;
const OFF_LAST_FOREGROUND_PID: usize = 68;
const E_PID: usize = 0;
const E_NAME: usize = 4;
const E_TIME0: usize = 268;
const E_TIME1: usize = 272;
const E_FRAMES: usize = 276;
/// `qwSimStartTime` e `qwPresentStartTime` (v2.21): `qwInputSampleTime` em 9232, logo depois de
/// `dwResolutionY` (9228), alinhado a 8.
const E_SIM_START: usize = 9240;
const E_PRESENT_START: usize = 9272;
const V2_21: u32 = 0x0002_0015;
const MAX_APPS: usize = 256;

/// Marcadores do Reflex (NV_LATENCY_MARKER_TYPE).
fn marker_name(m: u64) -> String {
    match m {
        0 => "SIM_START".into(),
        1 => "SIM_END".into(),
        2 => "RENDER_START".into(),
        3 => "RENDER_END".into(),
        4 => "PRESENT_START".into(),
        5 => "PRESENT_END".into(),
        6 => "INPUT_SAMPLE".into(),
        7 => "TRIGGER_FLASH".into(),
        8 => "PING".into(),
        9 => "OOB_RENDER_START".into(),
        10 => "OOB_RENDER_END".into(),
        11 => "OOB_PRESENT_START".into(),
        12 => "OOB_PRESENT_END".into(),
        n => format!("M{n}"),
    }
}

static STOP: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn on_ctrl(_: u32) -> windows::core::BOOL {
    STOP.store(true, Ordering::SeqCst);
    true.into()
}

// ---------------------------------------------------------------------------------------------
// Saída: console + arquivo

struct Out {
    file: Option<std::fs::File>,
}

static OUT: Mutex<Out> = Mutex::new(Out { file: None });

fn say(s: &str) {
    println!("{s}");
    let mut o = OUT.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(f) = o.file.as_mut() {
        let _ = writeln!(f, "{s}");
    }
}

macro_rules! say {
    ($($t:tt)*) => { say(&format!($($t)*)) };
}

// ---------------------------------------------------------------------------------------------
// Estado do consumidor ETW

/// Evento decodificado pelo TDH.
#[derive(Clone, Default)]
struct Info {
    provider: String,
    event: String,
    props: Vec<String>,
}

#[derive(Default)]
struct KeyStat {
    count: u64,
    /// Primeiros valores dos campos pequenos (inteiros), para ver o que cada evento carrega.
    samples: Vec<String>,
}

/// Contagens de um segundo (pelo carimbo de tempo do evento, não pela chegada do buffer).
#[derive(Default)]
struct Bin {
    dxgi_present: u64,
    markers: BTreeMap<u64, u64>,
    krnl: BTreeMap<String, u64>,
    /// Eventos dos providers da NVIDIA (driver, Reflex), por nome.
    nv: BTreeMap<String, u64>,
}

#[derive(Default)]
struct State {
    pid: u32,
    /// Fase 1: amostras de campos de todo provider que não é do Windows.
    sample_all: bool,
    cache: HashMap<(u128, u16, u8, u8, u16), Option<Info>>,
    keys: BTreeMap<(String, String), KeyStat>,
    /// Nome do provider -> GUID (para ligar na fase 2 só os que interessam).
    guids: BTreeMap<String, GUID>,
    events_total: u64,
    events_pid: u64,
    /// Início da fase (FILETIME, 100 ns): o segundo de cada evento é contado a partir dele.
    t0: i64,
    bins: BTreeMap<i64, Bin>,
}

/// Provider da NVIDIA ou de latência (onde estão os marcadores de frame real).
fn is_nv(provider: &str) -> bool {
    let l = provider.to_ascii_lowercase();
    provider.starts_with("NV")
        || ["nvidia", "pcl", "reflex", "latency", "streamline", "dlss"]
            .iter()
            .any(|k| l.contains(k))
}

/// Provider sem nome resolvido (o TDH não achou o manifesto): o nome é o próprio GUID.
fn is_unnamed(provider: &str) -> bool {
    provider.len() == 36 && provider.bytes().filter(|&b| b == b'-').count() == 4
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn wstr(buf: &[u8], off: u32) -> String {
    let off = off as usize;
    if off == 0 || off >= buf.len() {
        return String::new();
    }
    let w: Vec<u16> = buf[off..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .take_while(|&c| c != 0)
        .collect();
    String::from_utf16_lossy(&w).trim_end().to_string()
}

/// Decodifica o evento pelo TDH (nome do provider, do evento e dos campos de topo).
fn decode(r: &EVENT_RECORD) -> Option<Info> {
    let mut size = 0u32;
    // SAFETY: registro válido durante o callback; consulta de tamanho.
    let e = unsafe { TdhGetEventInformation(r, None, None, &raw mut size) };
    if e != ERROR_INSUFFICIENT_BUFFER.0 || size == 0 {
        return None;
    }
    let mut buf = vec![0u64; (size as usize).div_ceil(8)];
    let info = buf.as_mut_ptr().cast::<TRACE_EVENT_INFO>();
    // SAFETY: buffer alinhado com `size` bytes.
    if unsafe { TdhGetEventInformation(r, None, Some(info), &raw mut size) } != ERROR_SUCCESS.0 {
        return None;
    }
    // SAFETY: o TDH preencheu `size` bytes do buffer (alinhado a 8).
    let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), size as usize) };
    // SAFETY: cabeçalho preenchido pelo TDH.
    let i = unsafe { &*info };
    let provider = wstr(bytes, i.ProviderNameOffset);
    // SAFETY: união lida como o campo que o TDH preenche (nome do evento, Win10+).
    let mut event = wstr(bytes, unsafe { i.Anonymous1.EventNameOffset });
    if event.is_empty() {
        let task = wstr(bytes, i.TaskNameOffset);
        let op = wstr(bytes, i.OpcodeNameOffset);
        event = match (task.is_empty(), op.is_empty()) {
            (false, false) => format!("{task}/{op}"),
            (false, true) => task,
            (true, false) => op,
            (true, true) => format!("id{}", i.EventDescriptor.Id),
        };
    }
    let n = i.TopLevelPropertyCount as usize;
    let base = std::mem::offset_of!(TRACE_EVENT_INFO, EventPropertyInfoArray);
    let step = size_of::<windows::Win32::System::Diagnostics::Etw::EVENT_PROPERTY_INFO>();
    let mut props = Vec::new();
    for k in 0..n.min(32) {
        let off = base + k * step;
        if off + step > bytes.len() {
            break;
        }
        // SAFETY: entrada `k` do array dentro do buffer conferido acima.
        let p = unsafe {
            &*(bytes.as_ptr().add(off))
                .cast::<windows::Win32::System::Diagnostics::Etw::EVENT_PROPERTY_INFO>()
        };
        props.push(wstr(bytes, p.NameOffset));
    }
    Some(Info {
        provider,
        event,
        props,
    })
}

/// Valor inteiro (até 8 bytes) do campo `name`, se houver.
fn prop_int(r: &EVENT_RECORD, name: &str) -> Option<u64> {
    let w = common::wide(name);
    let d = [PROPERTY_DATA_DESCRIPTOR {
        PropertyName: w.as_ptr() as u64,
        ArrayIndex: u32::MAX,
        Reserved: 0,
    }];
    let mut size = 0u32;
    // SAFETY: registro válido; nome terminado em zero vivo durante as chamadas.
    if unsafe { TdhGetPropertySize(r, None, &d, &raw mut size) } != ERROR_SUCCESS.0
        || size == 0
        || size > 8
    {
        return None;
    }
    let mut b = [0u8; 8];
    // SAFETY: como acima; buffer local com `size` bytes.
    if unsafe { TdhGetProperty(r, None, &d, &mut b[..size as usize]) } != ERROR_SUCCESS.0 {
        return None;
    }
    Some(u64::from_le_bytes(b))
}

unsafe extern "system" fn on_event(r: *mut EVENT_RECORD) {
    // SAFETY: o ETW passa um registro válido durante o callback.
    let r = unsafe { &*r };
    let h = &r.EventHeader;
    let mut g = STATE.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(st) = g.as_mut() else {
        return;
    };
    st.events_total += 1;
    if h.ProcessId != st.pid {
        return;
    }
    st.events_pid += 1;
    let d = &h.EventDescriptor;
    // Segundo do evento (o carimbo é FILETIME: o consumidor não pediu o QPC cru).
    let sec = (h.TimeStamp - st.t0).div_euclid(10_000_000);
    if h.ProviderId == DXGI && d.Id == DXGI_PRESENT_START {
        st.bins.entry(sec).or_default().dxgi_present += 1;
    }
    let tl = d.Channel == CHANNEL_TRACELOGGING;
    let info = if tl {
        decode(r)
    } else {
        st.cache
            .entry((h.ProviderId.to_u128(), d.Id, d.Version, d.Opcode, d.Task))
            .or_insert_with(|| decode(r))
            .clone()
    };
    let info = info.unwrap_or_else(|| Info {
        provider: format!("{:?}", h.ProviderId),
        event: format!("id{}", d.Id),
        props: Vec::new(),
    });
    st.guids
        .entry(info.provider.clone())
        .or_insert(h.ProviderId);
    let lower = info.provider.to_ascii_lowercase();
    let nv = is_nv(&info.provider);
    let marker = if nv && info.props.iter().any(|p| p == "Marker") {
        prop_int(r, "Marker")
    } else {
        None
    };
    let bin = st.bins.entry(sec).or_default();
    if h.ProviderId == DXGKRNL {
        *bin.krnl.entry(info.event.clone()).or_default() += 1;
    }
    if nv {
        *bin.nv.entry(info.event.clone()).or_default() += 1;
    }
    if let Some(m) = marker {
        *bin.markers.entry(m).or_default() += 1;
    }
    let sample = (st.sample_all && !lower.starts_with("microsoft-windows-")) || nv;
    let key = (info.provider.clone(), info.event.clone());
    let k = st.keys.entry(key).or_default();
    k.count += 1;
    if sample && k.samples.len() < 3 {
        let mut s = String::new();
        for p in info.props.iter().take(12) {
            match prop_int(r, p) {
                Some(v) => {
                    let _ = write!(s, "{p}={v} ");
                }
                None => {
                    let _ = write!(s, "{p}=… ");
                }
            }
        }
        k.samples.push(s.trim_end().to_string());
    }
}

// ---------------------------------------------------------------------------------------------
// Sessão ETW

struct Session {
    handle: CONTROLTRACE_HANDLE,
    consumer: Option<std::thread::JoinHandle<()>>,
    thread: HANDLE,
}

fn props_buf() -> Vec<u64> {
    let name = common::wide(SESSION);
    let size = size_of::<EVENT_TRACE_PROPERTIES>() + name.len() * 2 + 1024;
    vec![0u64; size.div_ceil(8)]
}

fn props_of(buf: &mut [u64]) -> *mut EVENT_TRACE_PROPERTIES {
    let p = buf.as_mut_ptr().cast::<EVENT_TRACE_PROPERTIES>();
    // SAFETY: buffer alinhado e maior que a estrutura.
    unsafe {
        (*p).Wnode.BufferSize = (buf.len() * 8) as u32;
        (*p).LoggerNameOffset = size_of::<EVENT_TRACE_PROPERTIES>() as u32;
    }
    p
}

/// Para uma sessão com o nome do spike que tenha sobrado (Ctrl+C no meio de outra execução).
fn stop_leftover() {
    let mut buf = props_buf();
    let p = props_of(&mut buf);
    let name = common::wide(SESSION);
    // SAFETY: propriedades num buffer próprio; nome terminado em zero.
    let _ = unsafe {
        ControlTraceW(
            CONTROLTRACE_HANDLE::default(),
            PCWSTR(name.as_ptr()),
            p,
            EVENT_TRACE_CONTROL_STOP,
        )
    };
}

fn enable(s: &Session, guid: &GUID, level: u8, keywords: u64, pid: Option<u32>) -> WIN32_ERROR {
    let pids = [pid.unwrap_or(0)];
    let mut filter = EVENT_FILTER_DESCRIPTOR {
        Ptr: pids.as_ptr() as u64,
        Size: 4,
        Type: EVENT_FILTER_TYPE_PID,
    };
    let params = ENABLE_TRACE_PARAMETERS {
        Version: ENABLE_TRACE_PARAMETERS_VERSION_2,
        EnableFilterDesc: &raw mut filter,
        FilterDescCount: u32::from(pid.is_some()),
        ..Default::default()
    };
    // SAFETY: sessão aberta; parâmetros e filtro vivos durante a chamada.
    unsafe {
        EnableTraceEx2(
            s.handle,
            guid,
            EVENT_CONTROL_CODE_ENABLE_PROVIDER.0,
            level,
            keywords,
            0,
            0,
            Some(&raw const params),
        )
    }
}

fn start() -> Result<Session, String> {
    stop_leftover();
    let mut buf = props_buf();
    let p = props_of(&mut buf);
    // SAFETY: estrutura dentro do buffer próprio.
    unsafe {
        (*p).Wnode.Flags = WNODE_FLAG_TRACED_GUID;
        (*p).Wnode.ClientContext = 1; // QPC
        (*p).LogFileMode = EVENT_TRACE_REAL_TIME_MODE;
        (*p).FlushTimer = 1;
        (*p).BufferSize = 64;
        (*p).MinimumBuffers = 16;
        (*p).MaximumBuffers = 64;
    }
    let name = common::wide(SESSION);
    let mut handle = CONTROLTRACE_HANDLE::default();
    // SAFETY: propriedades com espaço para o nome; nome terminado em zero.
    let e = unsafe { StartTraceW(&raw mut handle, PCWSTR(name.as_ptr()), p) };
    if e == ERROR_ALREADY_EXISTS {
        return Err("sessão ETW já existe (rode de novo)".into());
    }
    if e != ERROR_SUCCESS {
        return Err(format!("StartTraceW: {e:?} (terminal elevado?)"));
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let consumer = std::thread::spawn(move || {
        let mut name = common::wide(SESSION);
        let mut log = EVENT_TRACE_LOGFILEW {
            LoggerName: PWSTR(name.as_mut_ptr()),
            ..Default::default()
        };
        log.Anonymous1.ProcessTraceMode =
            PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD;
        log.Anonymous2.EventRecordCallback = Some(on_event);
        // SAFETY: estrutura local preenchida; nome vivo até o fim da thread.
        let h = unsafe { OpenTraceW(&raw mut log) };
        let ok = h.Value != u64::MAX;
        let _ = tx.send(ok);
        if ok {
            // SAFETY: handle aberto acima; bloqueia até a sessão parar.
            let _ = unsafe { ProcessTrace(&[h], None::<*const FILETIME>, None::<*const FILETIME>) };
            // SAFETY: handle aberto acima.
            let _ = unsafe { CloseTrace(h) };
        }
    });
    let thread = HANDLE(consumer.as_raw_handle());
    if !rx.recv().unwrap_or(false) {
        return Err("OpenTraceW falhou".into());
    }
    Ok(Session {
        handle,
        consumer: Some(consumer),
        thread,
    })
}

impl Session {
    fn stop(&mut self) {
        let mut buf = props_buf();
        let p = props_of(&mut buf);
        // SAFETY: sessão iniciada por este processo; propriedades num buffer próprio.
        let _ = unsafe { ControlTraceW(self.handle, PCWSTR::null(), p, EVENT_TRACE_CONTROL_STOP) };
        if let Some(c) = self.consumer.take() {
            let _ = c.join();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Tempo de CPU (kernel + usuário) de uma thread, em 100 ns.
fn thread_cpu(h: HANDLE) -> u64 {
    let (mut a, mut b, mut k, mut u) = Default::default();
    // SAFETY: handle de thread válido; estruturas locais.
    if unsafe { GetThreadTimes(h, &raw mut a, &raw mut b, &raw mut k, &raw mut u) }.is_err() {
        return 0;
    }
    ft(k) + ft(u)
}

fn process_cpu() -> u64 {
    let (mut a, mut b, mut k, mut u) = Default::default();
    // SAFETY: pseudo-handle do processo atual; estruturas locais.
    if unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &raw mut a,
            &raw mut b,
            &raw mut k,
            &raw mut u,
        )
    }
    .is_err()
    {
        return 0;
    }
    ft(k) + ft(u)
}

fn ft(t: FILETIME) -> u64 {
    (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)
}

/// % do sistema inteiro (todos os lógicos), como o Gerenciador de Tarefas.
fn pct(cpu_100ns: u64, wall: Duration) -> f64 {
    let n = std::thread::available_parallelism().map_or(1, std::num::NonZero::get) as f64;
    cpu_100ns as f64 / 1e7 / wall.as_secs_f64() / n * 100.0
}

/// Providers com uma instância registrada no processo `pid`.
fn providers_of(pid: u32) -> Vec<GUID> {
    let mut size = 0u32;
    // SAFETY: consulta de tamanho.
    let _ = unsafe { EnumerateTraceGuidsEx(TraceGuidQueryList, None, 0, None, 0, &raw mut size) };
    let mut guids = vec![GUID::zeroed(); size as usize / size_of::<GUID>() + 16];
    // SAFETY: buffer com o tamanho informado.
    let e = unsafe {
        EnumerateTraceGuidsEx(
            TraceGuidQueryList,
            None,
            0,
            Some(guids.as_mut_ptr().cast()),
            (guids.len() * size_of::<GUID>()) as u32,
            &raw mut size,
        )
    };
    if e != ERROR_SUCCESS {
        say!("EnumerateTraceGuidsEx: {e:?}");
        return Vec::new();
    }
    guids.truncate(size as usize / size_of::<GUID>());
    let mut out = Vec::new();
    let mut buf = vec![0u64; 4096];
    for g in guids {
        let mut got = 0u32;
        // SAFETY: GUID de entrada vivo; buffer com o tamanho informado.
        let e = unsafe {
            EnumerateTraceGuidsEx(
                TraceGuidQueryInfo,
                Some((&raw const g).cast()),
                size_of::<GUID>() as u32,
                Some(buf.as_mut_ptr().cast()),
                (buf.len() * 8) as u32,
                &raw mut got,
            )
        };
        if e != ERROR_SUCCESS {
            continue;
        }
        // SAFETY: o buffer tem `got` bytes válidos (alinhado a 8).
        let b = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), got as usize) };
        let u32_at = |o: usize| {
            b.get(o..o + 4)
                .map(|x| u32::from_le_bytes(x.try_into().unwrap()))
        };
        // TRACE_GUID_INFO { InstanceCount, Reserved } + TRACE_PROVIDER_INSTANCE_INFO
        // { NextOffset, EnableCount, Pid, Flags } encadeadas.
        let count = u32_at(0).unwrap_or(0);
        let mut off = 8usize;
        for _ in 0..count.min(4096) {
            let (Some(next), Some(p)) = (u32_at(off), u32_at(off + 8)) else {
                break;
            };
            if p == pid {
                out.push(g);
                break;
            }
            if next == 0 {
                break;
            }
            off += next as usize;
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// RTSS

#[derive(Default)]
struct RtssSecond {
    fps: f64,
    sim_changes: u64,
    present_changes: u64,
    ok: bool,
}

static RTSS: Mutex<RtssSecond> = Mutex::new(RtssSecond {
    fps: 0.0,
    sim_changes: 0,
    present_changes: 0,
    ok: false,
});
static RTSS_CPU: AtomicU64 = AtomicU64::new(0);

/// Entrada do RTSS do `pid` (offset), se a Shared Memory tiver os campos da v2.21.
fn rtss_entry(v: &common::SharedView, pid: u32) -> Option<usize> {
    if v.u32_at(0)? != RTSS_SIGNATURE || v.u32_at(OFF_VERSION)? < V2_21 {
        return None;
    }
    let esize = v.u32_at(OFF_APP_ENTRY_SIZE)? as usize;
    if esize < E_PRESENT_START + 8 {
        return None;
    }
    let arr = v.u32_at(OFF_APP_ARR_OFFSET)? as usize;
    let count = (v.u32_at(OFF_APP_ARR_SIZE)? as usize).min(MAX_APPS);
    (0..count).find_map(|i| {
        let e = v.entry(arr, i, esize)?;
        (v.u32_at(e + E_PID)? == pid).then_some(e)
    })
}

/// Lê o RTSS a cada 1 ms: mudanças de `qwSimStartTime`/`qwPresentStartTime` por segundo.
fn rtss_loop(pid: u32) {
    // SAFETY: pseudo-handle da thread atual.
    let me = unsafe { GetCurrentThread() };
    let Some(v) = common::SharedView::open("RTSSSharedMemoryV2") else {
        return;
    };
    let mut entry = rtss_entry(&v, pid);
    let (mut last_sim, mut last_present) = (0u64, 0u64);
    let mut tick = Instant::now();
    let (mut sim, mut present) = (0u64, 0u64);
    while !STOP.load(Ordering::Relaxed) {
        if entry.is_none() && tick.elapsed() >= Duration::from_secs(1) {
            entry = rtss_entry(&v, pid);
        }
        if let Some(e) = entry {
            let s = v.u64_at(e + E_SIM_START).unwrap_or(0);
            let p = v.u64_at(e + E_PRESENT_START).unwrap_or(0);
            if s != 0 && s != last_sim {
                sim += 1;
                last_sim = s;
            }
            if p != 0 && p != last_present {
                present += 1;
                last_present = p;
            }
        }
        if tick.elapsed() >= Duration::from_secs(1) {
            tick = Instant::now();
            let mut r = RTSS.lock().unwrap_or_else(PoisonError::into_inner);
            r.sim_changes = sim;
            r.present_changes = present;
            r.ok = entry.is_some();
            r.fps = entry.map_or(0.0, |e| {
                let t0 = v.u32_at(e + E_TIME0).unwrap_or(0);
                let t1 = v.u32_at(e + E_TIME1).unwrap_or(0);
                let f = v.u32_at(e + E_FRAMES).unwrap_or(0);
                let span = t1.wrapping_sub(t0);
                if t0 != 0 && span > 0 && span < 60_000 {
                    1000.0 * f64::from(f) / f64::from(span)
                } else {
                    0.0
                }
            });
            (sim, present) = (0, 0);
            RTSS_CPU.store(thread_cpu(me), Ordering::Relaxed);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Jogo: PID dado na linha de comando, senão o último app 3D em primeiro plano do RTSS.
fn find_game(arg: Option<u32>) -> Option<(u32, String)> {
    let v = common::SharedView::open("RTSSSharedMemoryV2");
    let name_of = |pid: u32| -> String {
        v.as_ref()
            .and_then(|v| {
                let e = rtss_entry(v, pid).or_else(|| {
                    // Versão antiga: procura só pelo PID.
                    let esize = v.u32_at(OFF_APP_ENTRY_SIZE)? as usize;
                    let arr = v.u32_at(OFF_APP_ARR_OFFSET)? as usize;
                    (0..MAX_APPS).find_map(|i| {
                        let e = v.entry(arr, i, esize)?;
                        (v.u32_at(e + E_PID)? == pid).then_some(e)
                    })
                })?;
                v.cstr(e + E_NAME, 260)
            })
            .map_or_else(
                || "?".into(),
                |n| n.rsplit('\\').next().unwrap_or(&n).to_string(),
            )
    };
    if let Some(pid) = arg {
        return Some((pid, name_of(pid)));
    }
    let pid = v.as_ref()?.u32_at(OFF_LAST_FOREGROUND_PID)?;
    (pid != 0).then(|| (pid, name_of(pid)))
}

// ---------------------------------------------------------------------------------------------

fn now_filetime() -> i64 {
    // SAFETY: sem pré-condições.
    let t = unsafe { windows::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime() };
    ft(t) as i64
}

fn reset_state(pid: u32, sample_all: bool) {
    *STATE.lock().unwrap_or_else(PoisonError::into_inner) = Some(State {
        pid,
        sample_all,
        t0: now_filetime(),
        ..Default::default()
    });
}

/// Uma linha da fase 2: DXGI, marcadores, eventos NVIDIA e DxgKrnl de present/flip.
fn bin_row(b: &Bin) -> String {
    let list = |m: &BTreeMap<String, u64>, keep: &dyn Fn(&str) -> bool| -> String {
        let v: Vec<String> = m
            .iter()
            .filter(|(n, _)| keep(n))
            .map(|(n, c)| format!("{n} {c}"))
            .collect();
        if v.is_empty() {
            "-".into()
        } else {
            v.join(", ")
        }
    };
    let mk = if b.markers.is_empty() {
        "-".to_string()
    } else {
        b.markers
            .iter()
            .map(|(m, c)| format!("{} {c}", marker_name(*m)))
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        "DXGI {:>3} | {mk} | NV: {} | krnl: {}",
        b.dxgi_present,
        list(&b.nv, &|_| true),
        list(&b.krnl, &|n| n.starts_with("Present")),
    )
}

fn dump_keys(title: &str) {
    let g = STATE.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(st) = g.as_ref() else {
        return;
    };
    let total = st.events_total;
    let pid = st.events_pid;
    let mut rows: Vec<_> = st.keys.iter().collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.1.count));
    let rows: Vec<String> = rows
        .iter()
        .map(|((p, e), k)| {
            let mut s = format!("  {:>8}  {p} :: {e}", k.count);
            for x in &k.samples {
                let _ = write!(s, "\n            {x}");
            }
            s
        })
        .collect();
    drop(g);
    say!("\n{title}: {total} eventos na sessão, {pid} do jogo");
    for r in rows {
        say(&r);
    }
}

fn run(secs: u64, pid: u32, name: &str) -> Result<(), String> {
    say!("Jogo: PID {pid} {name}");
    let all = providers_of(pid);
    say!("Providers registrados no processo do jogo: {}", all.len());

    // Fase 1: descoberta.
    say!("\n== Fase 1: descoberta ({DESCOBERTA} s). Fique no jogo, com FG ligado ==");
    reset_state(pid, true);
    {
        let s = start()?;
        let mut on = 0;
        for g in &all {
            if enable(&s, g, 5, 0, None) == ERROR_SUCCESS {
                on += 1;
            }
        }
        let _ = enable(&s, &DXGKRNL, 5, DXGKRNL_KEYWORDS, None);
        say!("providers ligados: {on} + DxgKrnl");
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(DESCOBERTA) && !STOP.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    dump_keys("Descoberta");
    // Fase 2 só com o que interessa: providers da NVIDIA/latência e os sem nome resolvido.
    let chosen: Vec<(String, GUID)> = {
        let g = STATE.lock().unwrap_or_else(PoisonError::into_inner);
        g.as_ref().map_or_else(Vec::new, |st| {
            st.guids
                .iter()
                .filter(|(n, id)| (is_nv(n) || is_unnamed(n)) && **id != DXGI && **id != DXGKRNL)
                .map(|(n, id)| (n.clone(), *id))
                .collect()
        })
    };
    if STOP.load(Ordering::SeqCst) {
        return Ok(());
    }

    // Fase 2: medição.
    say!(
        "\n== Fase 2: medição ({secs} s, Ctrl+C encerra). Sugestão: FG ligado, desligado e \
         ligado de novo, uns 20 s cada =="
    );
    reset_state(pid, false);
    let rtss = std::thread::spawn(move || rtss_loop(pid));
    let s = start()?;
    let _ = enable(&s, &DXGI, 5, 0, Some(pid));
    let _ = enable(&s, &DXGKRNL, 5, DXGKRNL_KEYWORDS, None);
    for (_, g) in &chosen {
        let _ = enable(&s, g, 5, 0, Some(pid));
    }
    say!(
        "providers ligados: DXGI, DxgKrnl, {}",
        chosen
            .iter()
            .map(|(n, _)| n.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    say!(
        "colunas (2 s de atraso: o ETW entrega em blocos): t | RTSS fps e mudanças de sim/present \
         (Reflex) | DXGI Present | marcadores Reflex | eventos NVIDIA | DxgKrnl de present/flip"
    );
    let t0 = Instant::now();
    let cpu0 = process_cpu();
    let etw0 = thread_cpu(s.thread);
    let mut rtss_rows: Vec<String> = Vec::new();
    let mut printed = 0i64;
    let mut print_until = |upto: i64, rtss_rows: &[String]| {
        while printed < upto {
            let bin = {
                let mut g = STATE.lock().unwrap_or_else(PoisonError::into_inner);
                g.as_mut()
                    .and_then(|st| st.bins.remove(&printed))
                    .unwrap_or_default()
            };
            let rt = rtss_rows
                .get(printed as usize)
                .map_or("RTSS ?", String::as_str);
            say!("{:>3} | {rt} | {}", printed + 1, bin_row(&bin));
            printed += 1;
        }
    };
    while t0.elapsed() < Duration::from_secs(secs) && !STOP.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_secs(1));
        let r = RTSS.lock().unwrap_or_else(PoisonError::into_inner);
        rtss_rows.push(if r.ok {
            format!(
                "RTSS {:>5.1} fps sim {:>3} pres {:>3}",
                r.fps, r.sim_changes, r.present_changes
            )
        } else {
            "RTSS -".to_string()
        });
        drop(r);
        print_until(rtss_rows.len() as i64 - 2, &rtss_rows);
    }
    let wall = t0.elapsed();
    let etw = thread_cpu(s.thread).saturating_sub(etw0);
    drop(s);
    print_until(rtss_rows.len() as i64, &rtss_rows);
    STOP.store(true, Ordering::SeqCst);
    let _ = rtss.join();
    let total = process_cpu().saturating_sub(cpu0);
    let rtss_cpu = RTSS_CPU.load(Ordering::Relaxed);
    dump_keys("Medição (total do período)");
    say!(
        "\nCPU (% do sistema, {:.0} s): spike inteiro {:.3}%, consumidor ETW {:.3}%, leitura do \
         RTSS a 1 ms {:.3}%",
        wall.as_secs_f64(),
        pct(total, wall),
        pct(etw, wall),
        pct(rtss_cpu, wall)
    );
    say!(
        "(o consumidor ETW aqui decodifica com TDH; o custo dentro do jogo e do kernel não \
         entra nesta conta)"
    );
    Ok(())
}

fn pause() {
    print!("\nEnter para fechar.");
    let _ = std::io::stdout().flush();
    let _ = std::io::stdin().read_line(&mut String::new());
}

fn main() {
    common::harden_dll_search();
    let mut args = std::env::args().skip(1);
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(90);
    let pid_arg: Option<u32> = args.next().and_then(|s| s.parse().ok());
    if !common::is_elevated() {
        println!("Precisa de terminal elevado (sessão ETW): abra como administrador.");
        pause();
        return;
    }
    // SAFETY: handler estático; só marca a parada.
    let _ = unsafe { SetConsoleCtrlHandler(Some(on_ctrl), true) };
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let path = format!("spike_framegen_{stamp}.txt");
    OUT.lock().unwrap_or_else(PoisonError::into_inner).file = std::fs::File::create(&path).ok();
    say!("spike_framegen: log em {path}");
    say!("Abra o jogo (DLSS FG e Reflex ligados) e deixe-o em primeiro plano.");
    let mut game = None;
    for i in 0..60 {
        if STOP.load(Ordering::SeqCst) {
            break;
        }
        game = find_game(pid_arg);
        if let Some((_, n)) = &game
            && n != "?"
        {
            break;
        }
        if i % 5 == 0 {
            say!("esperando o jogo no RTSS (último app 3D em primeiro plano)...");
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    match game {
        Some((pid, name)) => {
            if let Err(e) = run(secs, pid, &name) {
                say!("erro: {e}");
            }
        }
        None => say!("jogo não encontrado (RTSS aberto? ou passe o PID: spike_framegen 90 <pid>)"),
    }
    say!("\nFim. Mande o arquivo {path}.");
    pause();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets_do_reflex_v2_21() {
        // dwResolutionY (9228) + 4 = 9232: qwInputSampleTime; sim = +8, present = +5*8.
        assert_eq!(E_SIM_START, 9232 + 8);
        assert_eq!(E_PRESENT_START, 9232 + 5 * 8);
        // dwGpuActiveRenderTime (9336) vem depois dos 13 ULONGLONG.
        assert_eq!(9232 + 13 * 8, 9336);
    }

    #[test]
    fn nome_dos_marcadores() {
        assert_eq!(marker_name(0), "SIM_START");
        assert_eq!(marker_name(4), "PRESENT_START");
        assert_eq!(marker_name(99), "M99");
    }

    #[test]
    fn texto_utf16_do_tdh() {
        let mut b = vec![0u8; 4];
        for c in "Abc".encode_utf16().chain([0]) {
            b.extend_from_slice(&c.to_le_bytes());
        }
        assert_eq!(wstr(&b, 4), "Abc");
        assert_eq!(wstr(&b, 0), "");
        assert_eq!(wstr(&b, 999), "");
    }
}
