//! FPS pelo RTSS (thread própria): Shared Memory `RTSSSharedMemoryV2` e pipe de frametime.
//!
//! - Shared Memory (v2.x, offsets do MSVC a partir do header oficial; campos novos só se a
//!   versão e o tamanho da entrada os cobrirem): jogo, API, resolução, tempos de GPU.
//! - Pipe `\\.\pipe\RTSS_Frametime`: um registro `{dwApp, dwFrametime}` por frame (`dwApp` =
//!   índice da entrada na Shared Memory; µs). 8 instâncias criadas por nós, DACL restrita,
//!   rótulo de integridade média, overlapped, sem clientes remotos. O hook do RTSS só conecta no
//!   início do jogo.
//! - Jogo aberto antes do wintop: frametime amostrado na Shared Memory a cada 250 ms, marcado
//!   como aproximado (lows só como ordem de grandeza).
//! - Lows (1% e 0,1%) calculados pelo wintop numa janela de 60 s (os do RTSS só existem no
//!   modo benchmark).

use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    ERROR_IO_PENDING, ERROR_MORE_DATA, ERROR_NO_DATA, ERROR_OPERATION_ABORTED,
    ERROR_PIPE_CONNECTED, HANDLE, HLOCAL, LocalFree, WAIT_OBJECT_0,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows::Win32::Storage::FileSystem::{
    FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_FLAGS_AND_ATTRIBUTES,
    PIPE_ACCESS_DUPLEX, ReadFile,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_MESSAGE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_MESSAGE, PIPE_WAIT,
};
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, OpenProcessToken, ResetEvent, SetEvent,
    WaitForMultipleObjects, WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR, w};

use crate::model::{Fps, Game, Metric, Missing, fail, neutral};
use crate::shm::SharedView;
use crate::win::{self, own, wide};

// RTSS_SHARED_MEMORY
const SHM_NAME: &str = "RTSSSharedMemoryV2";
const RTSS_SIGNATURE: u32 = 0x5254_5353; // 'RTSS'
const RTSS_DEAD: u32 = 0xDEAD;
const OFF_VERSION: usize = 4;
const OFF_APP_ENTRY_SIZE: usize = 8;
const OFF_APP_ARR_OFFSET: usize = 12;
const OFF_APP_ARR_SIZE: usize = 16;
const OFF_LAST_FOREGROUND_PID: usize = 68; // v2.16+
// RTSS_SHARED_MEMORY_APP_ENTRY (offsets do compilador MSVC, header v2.21)
const E_PID: usize = 0;
const E_NAME: usize = 4;
const E_FLAGS: usize = 264;
const E_TIME0: usize = 268;
const E_TIME1: usize = 272;
const E_FRAMES: usize = 276;
const E_FRAME_TIME: usize = 280;
const E_RES_X: usize = 9224; // v2.20+
const E_RES_Y: usize = 9228;
const E_GPU_ACTIVE: usize = 9336; // v2.21+
const E_GPU_FRAME: usize = 9340;
const V2_16: u32 = 0x0002_0010;
const V2_20: u32 = 0x0002_0014;
const V2_21: u32 = 0x0002_0015;
const MAX_APPS: usize = 256;

/// Processos que o RTSS acompanha mas não são jogos (overlays, navegadores, apps Electron).
/// Na Etapa 7 vira configuração.
const IGNORED: &[&str] = &[
    "claude.exe",
    "spotify.exe",
    "spotifast.exe",
    "zapfast.exe",
    "discord.exe",
    "chrome.exe",
    "msedge.exe",
    "firefox.exe",
    "msedgewebview2.exe",
    "steamwebhelper.exe",
    "gameoverlayui64.exe",
    "openlogi-overlay.exe",
    "yourphoneappproxyhost.exe",
];
/// Idade máxima do último período de FPS para o app contar como "ativo".
const ACTIVE_MS: u32 = 2000;
/// Janela dos lows.
const WINDOW: Duration = Duration::from_secs(60);
/// Recalcular os lows no máximo uma vez por segundo (ordenar ~9 mil frames).
const LOWS_EVERY: Duration = Duration::from_secs(1);
/// GPU ocupada nesta fração do frame (ou mais) = limitado pela GPU.
const GPU_BOUND: f64 = 0.9;
/// Depois de iniciar o RTSS, quanto esperar a Shared Memory antes de chamar de falha.
const RTSS_START_GRACE: Duration = Duration::from_secs(15);
/// Sem Shared Memory depois de iniciar: nova tentativa a cada minuto.
const RTSS_START_RETRY: Duration = Duration::from_secs(60);
const RTSS_KEY: &str = r"SOFTWARE\Unwinder\RTSS";

fn api_name(flags: u32) -> &'static str {
    match flags & 0xFFFF {
        0x1 => "OpenGL",
        0x2 => "DirectDraw",
        0x3 => "D3D8",
        0x4 => "D3D9",
        0x5 => "D3D9Ex",
        0x6 => "D3D10",
        0x7 => "D3D11",
        0x8 => "D3D12",
        0x9 => "D3D12 AFR",
        0xA => "Vulkan",
        0 => "none",
        _ => "unknown API",
    }
}

#[derive(Clone, Debug, Default)]
struct App {
    /// Índice da entrada na Shared Memory (= `dwApp` do pipe).
    index: u32,
    pid: u32,
    name: String,
    api: u32,
    fps: f64,
    frametime_us: u32,
    /// ms desde o fim do último período de FPS; `None` = nenhum período registrado.
    age_ms: Option<u32>,
    res: Option<(u32, u32)>,
    gpu: Option<(u32, u32)>,
}

/// Idade sem underflow: `now` é lido depois de `t1`; diferença "negativa" = 0.
fn age_ms(now: u32, t1: u32) -> Option<u32> {
    if t1 == 0 {
        return None;
    }
    let d = now.wrapping_sub(t1);
    Some(if d > i32::MAX as u32 { 0 } else { d })
}

/// Aplicativos da Shared Memory, PID em primeiro plano e versão.
fn read_apps(v: &SharedView) -> Result<(Vec<App>, Option<u32>, u32), String> {
    let oob = || "RTSS shared memory read out of bounds".to_string();
    let sig = v.u32_at(0).ok_or_else(oob)?;
    if sig == RTSS_DEAD {
        return Err("RTSS is shutting down".into());
    }
    if sig != RTSS_SIGNATURE {
        return Err(format!("invalid RTSS signature {sig:#x}"));
    }
    let ver = v.u32_at(OFF_VERSION).ok_or_else(oob)?;
    if ver >> 16 != 2 {
        return Err(format!(
            "unsupported RTSS shared memory {}.{}",
            ver >> 16,
            ver & 0xFFFF
        ));
    }
    let esize = v.u32_at(OFF_APP_ENTRY_SIZE).ok_or_else(oob)? as usize;
    let arr = v.u32_at(OFF_APP_ARR_OFFSET).ok_or_else(oob)? as usize;
    let count = (v.u32_at(OFF_APP_ARR_SIZE).ok_or_else(oob)? as usize).min(MAX_APPS);
    if esize < E_FRAME_TIME + 4 {
        return Err(format!("RTSS app entry of {esize} bytes is too small"));
    }
    let has_res = ver >= V2_20 && esize >= E_RES_Y + 4;
    let has_gpu = ver >= V2_21 && esize >= E_GPU_FRAME + 4;
    let fg = if ver >= V2_16 {
        v.u32_at(OFF_LAST_FOREGROUND_PID)
    } else {
        None
    };
    let mut apps = Vec::new();
    for i in 0..count {
        let e = v.entry(arr, i, esize).ok_or_else(oob)?;
        let pid = v.u32_at(e + E_PID).ok_or_else(oob)?;
        if pid == 0 {
            continue;
        }
        let name = v.cstr(e + E_NAME, 260).ok_or_else(oob)?;
        let name = name.rsplit('\\').next().unwrap_or(&name).to_string();
        let t0 = v.u32_at(e + E_TIME0).ok_or_else(oob)?;
        let t1 = v.u32_at(e + E_TIME1).ok_or_else(oob)?;
        let frames = v.u32_at(e + E_FRAMES).ok_or_else(oob)?;
        // SAFETY: sem pré-condições; lido depois de `t1` (ver `age_ms`).
        let now = unsafe { GetTickCount() };
        let span = t1.wrapping_sub(t0);
        let fps = if t0 != 0 && span > 0 && span < 60_000 {
            1000.0 * f64::from(frames) / f64::from(span)
        } else {
            0.0
        };
        let pair = |a: usize, b: usize| -> Result<(u32, u32), String> {
            Ok((
                v.u32_at(e + a).ok_or_else(oob)?,
                v.u32_at(e + b).ok_or_else(oob)?,
            ))
        };
        apps.push(App {
            index: i as u32,
            pid,
            name,
            api: v.u32_at(e + E_FLAGS).ok_or_else(oob)? & 0xFFFF,
            fps,
            frametime_us: v.u32_at(e + E_FRAME_TIME).ok_or_else(oob)?,
            age_ms: age_ms(now, t1),
            res: if has_res {
                Some(pair(E_RES_X, E_RES_Y)?)
            } else {
                None
            },
            gpu: if has_gpu {
                Some(pair(E_GPU_ACTIVE, E_GPU_FRAME)?)
            } else {
                None
            },
        });
    }
    Ok((apps, fg, ver))
}

/// Jogo atual: ativo, com API 3D e fora da lista de ignorados; o primeiro plano desempata,
/// senão o de maior FPS.
fn select_game(apps: &[App], fg: Option<u32>) -> Option<&App> {
    let candidates: Vec<&App> = apps
        .iter()
        .filter(|a| a.api != 0 && a.age_ms.is_some_and(|m| m < ACTIVE_MS))
        .filter(|a| !IGNORED.contains(&a.name.to_ascii_lowercase().as_str()))
        .collect();
    candidates
        .iter()
        .find(|a| Some(a.pid) == fg)
        .or_else(|| candidates.iter().max_by(|a, b| a.fps.total_cmp(&b.fps)))
        .copied()
}

/// (1% low, 0,1% low) em FPS: média dos frametimes do 1% (0,1%) mais lento. Exige 100
/// amostras; com menos de 1000, o 0,1% usa o pior frame.
fn lows(samples_us: impl Iterator<Item = u32>) -> Option<(f64, f64)> {
    let mut s: Vec<u32> = samples_us.filter(|&u| u > 0).collect();
    if s.len() < 100 {
        return None;
    }
    s.sort_unstable_by(|a, b| b.cmp(a));
    let avg_fps = |k: usize| {
        let k = k.clamp(1, s.len());
        let avg = s[..k].iter().map(|&u| u64::from(u)).sum::<u64>() as f64 / k as f64;
        (avg > 0.0).then(|| 1e6 / avg)
    };
    Some((avg_fps(s.len() / 100)?, avg_fps(s.len() / 1000)?))
}

/// Frames de um app (pipe, por índice da Shared Memory, ou amostras aproximadas, por PID).
#[derive(Default)]
struct Ring {
    /// PID dono do índice (o índice volta a ser usado quando o app fecha).
    pid: Option<u32>,
    frames: VecDeque<(Instant, u32)>,
}

impl Ring {
    fn push(&mut self, at: Instant, us: u32) {
        self.frames.push_back((at, us));
    }

    fn prune(&mut self, now: Instant) {
        while self
            .frames
            .front()
            .is_some_and(|(t, _)| now.duration_since(*t) > WINDOW)
        {
            self.frames.pop_front();
        }
    }

    /// Há frames nos últimos `within`.
    fn recent(&self, now: Instant, within: Duration) -> bool {
        self.frames
            .back()
            .is_some_and(|(t, _)| now.duration_since(*t) <= within)
    }

    /// Frametimes do último segundo.
    fn last_second(&self, now: Instant) -> impl Iterator<Item = u32> + '_ {
        self.frames
            .iter()
            .rev()
            .take_while(move |(t, _)| now.duration_since(*t) <= Duration::from_secs(1))
            .map(|(_, us)| *us)
    }
}

/// Registros completos de 8 bytes; a sobra de uma mensagem partida (`ERROR_MORE_DATA`) fica em
/// `carry`; sobra numa mensagem completa (malformada) é descartada.
fn feed(rings: &mut HashMap<u32, Ring>, carry: &mut Vec<u8>, data: &[u8], complete: bool) {
    carry.extend_from_slice(data);
    let whole = carry.len() / 8 * 8;
    let now = Instant::now();
    for rec in carry[..whole].as_chunks::<8>().0 {
        let app = u32::from_le_bytes([rec[0], rec[1], rec[2], rec[3]]);
        let us = u32::from_le_bytes([rec[4], rec[5], rec[6], rec[7]]);
        // Índice fora da Shared Memory ou frametime 0 (sem delta): registro ignorado.
        if (app as usize) < MAX_APPS && us > 0 {
            rings.entry(app).or_default().push(now, us);
        }
    }
    carry.drain(..whole);
    if complete {
        carry.clear();
    }
}

// --- Pipe ---

const INSTANCES: usize = 8;
const BUF: usize = 64 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pending {
    None,
    Connect,
    /// Cliente conectou antes do `ConnectNamedPipe`: evento sinalizado à mão.
    Connected,
    /// Rearmar a conexão no próximo ciclo (ex.: `ERROR_NO_DATA`), sem recursão.
    Rearm,
    Read,
}

/// Uma instância do pipe. `ov` e `buf` em Box: o endereço não muda com I/O pendente. Toda
/// conclusão passa pelo evento e é tratada no laço (`on_signal`), uma por vez.
struct Instance {
    pipe: OwnedHandle,
    event: OwnedHandle,
    ov: Box<OVERLAPPED>,
    buf: Box<[u8]>,
    carry: Vec<u8>,
    pending: Pending,
    connected: bool,
}

fn h(o: &OwnedHandle) -> HANDLE {
    HANDLE(o.as_raw_handle())
}

impl Instance {
    fn create(first: bool, sa: &SECURITY_ATTRIBUTES) -> Result<Self, String> {
        let mut mode: FILE_FLAGS_AND_ATTRIBUTES = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
        if first {
            mode |= FILE_FLAG_FIRST_PIPE_INSTANCE; // falha se outro servidor já existe
        }
        // SAFETY: atributos válidos durante a chamada.
        let pipe = unsafe {
            CreateNamedPipeW(
                w!(r"\\.\pipe\RTSS_Frametime"),
                mode,
                PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                INSTANCES as u32, // limite fixo: ninguém cria instâncias além das nossas
                BUF as u32,
                BUF as u32,
                0,
                Some(sa),
            )
        };
        if pipe.is_invalid() {
            return Err(format!(
                "CreateNamedPipe: {}",
                windows::core::Error::from_thread()
            ));
        }
        // SAFETY: handle válido recém-criado, com um único dono.
        let pipe = unsafe { own(pipe) };
        // SAFETY: evento manual sem nome.
        let event = unsafe { CreateEventW(None, true, false, None) }
            .map_err(|e| format!("CreateEvent: {e}"))?;
        // SAFETY: handle válido recém-criado, com um único dono.
        let event = unsafe { own(event) };
        let ov = Box::new(OVERLAPPED {
            hEvent: h(&event),
            ..Default::default()
        });
        Ok(Self {
            pipe,
            event,
            ov,
            buf: vec![0u8; BUF].into_boxed_slice(),
            carry: Vec::new(),
            pending: Pending::None,
            connected: false,
        })
    }

    fn fresh_ov(&mut self) {
        *self.ov = OVERLAPPED {
            hEvent: h(&self.event),
            ..Default::default()
        };
    }

    fn signal(&self) {
        // SAFETY: evento válido.
        unsafe {
            let _ = SetEvent(h(&self.event));
        }
    }

    fn start_connect(&mut self) {
        self.connected = false;
        self.carry.clear();
        self.fresh_ov();
        // SAFETY: `ov` (em Box) vive até a conclusão ou o cancelamento.
        match unsafe { ConnectNamedPipe(h(&self.pipe), Some(&raw mut *self.ov)) } {
            Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => self.pending = Pending::Connect,
            Ok(()) => {
                self.pending = Pending::Connected;
                self.signal();
            }
            Err(e) if e.code() == ERROR_PIPE_CONNECTED.to_hresult() => {
                self.pending = Pending::Connected;
                self.signal();
            }
            Err(e) if e.code() == ERROR_NO_DATA.to_hresult() => {
                // O cliente conectou e fechou antes: desconecta e rearma no próximo ciclo.
                // SAFETY: pipe válido; sem I/O pendente.
                unsafe {
                    let _ = DisconnectNamedPipe(h(&self.pipe));
                }
                self.pending = Pending::Rearm;
                self.signal();
            }
            Err(_) => self.pending = Pending::None, // instância desativada
        }
    }

    fn reconnect(&mut self) {
        // SAFETY: pipe válido; sem I/O pendente aqui.
        unsafe {
            let _ = DisconnectNamedPipe(h(&self.pipe));
        }
        self.start_connect();
    }

    fn start_read(&mut self) {
        self.fresh_ov();
        // SAFETY: `buf` e `ov` (em Box) vivem até a conclusão ou o cancelamento.
        let r = unsafe {
            ReadFile(
                h(&self.pipe),
                Some(&mut self.buf),
                None,
                Some(&raw mut *self.ov),
            )
        };
        match r {
            Ok(()) => self.pending = Pending::Read,
            Err(e)
                if e.code() == ERROR_IO_PENDING.to_hresult()
                    || e.code() == ERROR_MORE_DATA.to_hresult() =>
            {
                self.pending = Pending::Read;
            }
            Err(_) => {
                self.pending = Pending::None;
                self.reconnect();
            }
        }
    }

    /// O evento sinalizou: trata uma conclusão e arma a próxima operação.
    fn on_signal(&mut self, rings: &mut HashMap<u32, Ring>) {
        // SAFETY: evento válido; a próxima operação (ou `signal`) volta a sinalizá-lo.
        unsafe {
            let _ = ResetEvent(h(&self.event));
        }
        let state = std::mem::replace(&mut self.pending, Pending::None);
        let mut n = 0u32;
        match state {
            Pending::None => {}
            Pending::Rearm => self.start_connect(),
            Pending::Connected => {
                self.connected = true;
                self.start_read();
            }
            Pending::Connect => {
                // SAFETY: conexão concluída sobre `ov`.
                let r = unsafe {
                    GetOverlappedResult(h(&self.pipe), &raw const *self.ov, &raw mut n, false)
                };
                if r.is_ok() {
                    self.connected = true;
                    self.start_read();
                } else {
                    self.reconnect();
                }
            }
            Pending::Read => {
                // SAFETY: leitura concluída sobre `ov` (o evento sinalizou).
                let r = unsafe {
                    GetOverlappedResult(h(&self.pipe), &raw const *self.ov, &raw mut n, false)
                };
                let got = (n as usize).min(self.buf.len());
                match r {
                    Ok(()) => {
                        feed(rings, &mut self.carry, &self.buf[..got], true);
                        self.start_read();
                    }
                    Err(e) if e.code() == ERROR_MORE_DATA.to_hresult() => {
                        feed(rings, &mut self.carry, &self.buf[..got], false);
                        self.start_read();
                    }
                    Err(e) if e.code() == ERROR_OPERATION_ABORTED.to_hresult() => {}
                    Err(_) => self.reconnect(),
                }
            }
        }
    }

    /// Cancela o I/O pendente e espera a conclusão (nunca solta `ov`/`buf` com I/O no kernel).
    fn cancel(&mut self) {
        if !matches!(self.pending, Pending::Connect | Pending::Read) {
            return;
        }
        let mut n = 0u32;
        // SAFETY: cancela a operação sobre `ov` e espera a conclusão antes de soltar o Box.
        unsafe {
            let _ = CancelIoEx(h(&self.pipe), Some(&raw const *self.ov));
            let _ = GetOverlappedResult(h(&self.pipe), &raw const *self.ov, &raw mut n, true);
        }
        self.pending = Pending::None;
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// SID do usuário do token atual, em texto (S-1-5-21-...).
fn user_sid() -> Option<String> {
    let mut token = HANDLE::default();
    // SAFETY: token passa a ter dono logo abaixo.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) }.ok()?;
    // SAFETY: OpenProcessToken teve sucesso; handle válido e sem outro dono.
    let _token = unsafe { own(token) };
    let mut buf = vec![0u64; 64]; // alinhado para TOKEN_USER
    let mut len = 0u32;
    // SAFETY: buffer local com o tamanho informado.
    unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr().cast::<c_void>()),
            (buf.len() * 8) as u32,
            &raw mut len,
        )
    }
    .ok()?;
    // SAFETY: GetTokenInformation preencheu um TOKEN_USER no início do buffer alinhado.
    let user = unsafe { buf.as_ptr().cast::<TOKEN_USER>().read() };
    let mut text = PWSTR::null();
    // SAFETY: SID válido enquanto `buf` vive; o texto é liberado com LocalFree.
    unsafe { ConvertSidToStringSidW(user.User.Sid, &raw mut text) }.ok()?;
    // SAFETY: `text` aponta para uma string terminada em zero alocada pela API.
    let s = unsafe { text.to_string() }.ok();
    // SAFETY: liberado uma única vez.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(text.0.cast())));
    }
    s
}

/// As 8 instâncias do pipe, ou erro. Todas ou nenhuma: uma vaga livre deixaria outro processo
/// do usuário criar uma instância e receber conexões do hook (GW inclui CREATE_PIPE_INSTANCE).
fn create_pipe() -> Result<Vec<Instance>, String> {
    let sid = user_sid().ok_or("could not read the user SID")?;
    // DACL protegida: SYSTEM e Administradores com controle total; o usuário (jogo sem
    // elevação) com GRGW, sem WRITE_DAC/WRITE_OWNER. Rótulo de integridade média.
    let sddl = wide(&format!(
        "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;{sid})S:(ML;;NW;;;ME)"
    ));
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: SDDL terminado em zero; o descritor é liberado abaixo.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &raw mut sd,
            None,
        )
    }
    .map_err(|e| format!("pipe SDDL: {e}"))?;
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: false.into(),
    };
    let instances: Result<Vec<Instance>, String> = (0..INSTANCES)
        .map(|i| Instance::create(i == 0, &sa))
        .collect();
    // SAFETY: descritor alocado acima; as instâncias já foram criadas.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(sd.0)));
    }
    let mut instances =
        instances.map_err(|e| format!("frametime pipe (another server running?): {e}"))?;
    for inst in &mut instances {
        inst.start_connect();
    }
    Ok(instances)
}

// --- Coletor ---

/// `Fps` com a mesma falha em tudo (pânico ou início impossível da thread).
pub fn failed(cause: &str) -> Fps {
    let m = Missing::Fail(cause.to_string());
    Fps {
        rtss: Err(m.clone()),
        pipe: Err(m.clone()),
        game: Err(m),
    }
}

/// Lows em cache: (instante, PID, aproximado, valor).
type LowsCache = Option<(Instant, u32, bool, Metric<(f64, f64)>)>;

pub struct RtssCollector {
    view: Option<SharedView>,
    /// Se o RTSS deve ser iniciado quando ausente (modo normal; nunca no `--check`).
    start_rtss: bool,
    started_at: Option<Instant>,
    start_error: Option<String>,
    /// `Err(None)`: pipe desligado de propósito (`--check`); `Err(Some(e))`: falhou.
    pipe: Result<Vec<Instance>, Option<String>>,
    turn: usize,
    /// Frames do pipe por índice da Shared Memory.
    exact: HashMap<u32, Ring>,
    /// Amostras aproximadas da Shared Memory por PID.
    approx: HashMap<u32, Ring>,
    lows: LowsCache,
}

impl RtssCollector {
    /// `pipe`: criar o servidor do pipe (só no modo normal: o `--check` não espera frames).
    pub fn new(start_rtss: bool, pipe: bool) -> Self {
        Self {
            view: None,
            start_rtss,
            started_at: None,
            start_error: None,
            pipe: if pipe {
                create_pipe().map_err(Some)
            } else {
                Err(None)
            },
            turn: 0,
            exact: HashMap::new(),
            approx: HashMap::new(),
            lows: None,
        }
    }

    /// Atende o pipe até `deadline` (conclusões de conexão e leitura). Volta na hora se `stop`
    /// (evento manual) for sinalizado; devolve `true` nesse caso.
    pub fn pump(&mut self, deadline: Instant, stop: HANDLE) -> bool {
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            let timeout = left.as_millis().min(u128::from(u32::MAX - 1)) as u32;
            let Ok(instances) = &mut self.pipe else {
                // SAFETY: evento válido (dono: o motor, que espera esta thread terminar).
                return unsafe { WaitForSingleObject(stop, timeout) } == WAIT_OBJECT_0;
            };
            let armed: Vec<usize> = (0..instances.len())
                .map(|k| (k + self.turn) % instances.len())
                .filter(|&k| instances[k].pending != Pending::None)
                .collect();
            self.turn = self.turn.wrapping_add(1);
            // O evento de parada vem primeiro (índice 0); depois os das instâncias armadas.
            let mut events = vec![stop];
            events.extend(armed.iter().map(|&k| h(&instances[k].event)));
            // SAFETY: eventos válidos (no máximo 9).
            let r = unsafe { WaitForMultipleObjects(&events, false, timeout) };
            let idx = r.0.wrapping_sub(WAIT_OBJECT_0.0) as usize;
            if idx == 0 {
                return true;
            }
            match armed.get(idx - 1) {
                Some(&k) => instances[k].on_signal(&mut self.exact),
                None => return false, // timeout ou erro
            }
        }
    }

    /// `None` com a Shared Memory aberta; senão o estado do RTSS (iniciando ou falha).
    fn open_view(&mut self) -> Option<Missing> {
        if self.view.is_none() {
            self.view = SharedView::open(SHM_NAME);
        }
        if self.view.is_some() {
            self.started_at = None;
            self.start_error = None;
            return None;
        }
        if !self.start_rtss {
            return Some(Missing::Fail("RTSS not running".into()));
        }
        // Nova tentativa de iniciar depois de `RTSS_START_RETRY`.
        if self
            .started_at
            .is_none_or(|t| t.elapsed() >= RTSS_START_RETRY)
        {
            self.started_at = Some(Instant::now());
            self.start_error = start_rtss().err();
        }
        if let Some(e) = &self.start_error {
            return Some(Missing::Fail(format!("RTSS not running: {e}")));
        }
        Some(
            if self
                .started_at
                .is_some_and(|t| t.elapsed() < RTSS_START_GRACE)
            {
                Missing::Neutral("starting RTSS".into())
            } else {
                Missing::Fail("RTSS not running (started, but no shared memory yet)".into())
            },
        )
    }

    /// Sem Shared Memory válida os índices do pipe não têm dono: descarta tudo.
    fn forget_frames(&mut self) {
        self.exact.clear();
        self.approx.clear();
        self.lows = None;
    }

    /// Uma amostra: estado do RTSS e do jogo.
    pub fn sample(&mut self) -> Fps {
        let pipe = match &self.pipe {
            Ok(list) => Ok((list.len(), list.iter().filter(|i| i.connected).count())),
            Err(Some(e)) => fail(e.clone()),
            Err(None) => neutral("not started in --check"),
        };
        if let Some(missing) = self.open_view() {
            self.forget_frames();
            return Fps {
                rtss: Err(missing.clone()),
                pipe,
                game: Err(missing),
            };
        }
        let read = self.view.as_ref().map(read_apps);
        let (apps, fg, ver) = match read {
            Some(Ok(x)) => x,
            Some(Err(e)) => {
                // 0xDEAD ou lixo: desmapeia e reconecta na próxima amostra.
                self.view = None;
                self.forget_frames();
                return Fps {
                    rtss: fail(e.clone()),
                    pipe,
                    game: fail(e),
                };
            }
            None => unreachable!("view aberta em open_view"),
        };
        let rtss = Ok(format!("shared memory {}.{}", ver >> 16, ver & 0xFFFF));
        let now = Instant::now();
        // Índice reaproveitado por outro app: os frames antigos não valem. Ring sem dono
        // (frames que chegaram antes desta amostra) fica com o app atual do índice.
        for a in &apps {
            let ring = self.exact.entry(a.index).or_default();
            if ring.pid.is_some_and(|p| p != a.pid) {
                ring.frames.clear();
            }
            ring.pid = Some(a.pid);
        }
        self.exact.retain(|i, _| apps.iter().any(|a| a.index == *i));
        self.approx
            .retain(|pid, _| apps.iter().any(|a| a.pid == *pid));
        for r in self.exact.values_mut().chain(self.approx.values_mut()) {
            r.prune(now);
        }
        let game = match select_game(&apps, fg) {
            None => neutral("no 3D game in focus"),
            Some(g) => Ok(self.game(g, now)),
        };
        Fps { rtss, pipe, game }
    }

    fn game(&mut self, g: &App, now: Instant) -> Game {
        // Exato enquanto o pipe trouxe frames na janela de atividade (um travamento de 1-2 s
        // não vira "approx." nem perde os lows exatos).
        let exact_recent = self
            .exact
            .get(&g.index)
            .is_some_and(|r| r.recent(now, Duration::from_millis(u64::from(ACTIVE_MS))));
        let approx = !exact_recent;
        if approx && g.frametime_us > 0 {
            self.approx
                .entry(g.pid)
                .or_default()
                .push(now, g.frametime_us);
        }
        let ring = if approx {
            self.approx.get(&g.pid)
        } else {
            self.exact.get(&g.index)
        };
        let second: Vec<u32> = ring.map_or_else(Vec::new, |r| {
            r.last_second(now).filter(|&u| u > 0).collect()
        });
        let (fps, frametime) = if approx {
            // FPS do período do RTSS; frametime: o instantâneo e os extremos das amostras.
            let cur = f64::from(g.frametime_us) / 1000.0;
            let (lo, hi) = min_max_ms(&second).unwrap_or((cur, cur));
            (positive(g.fps), positive(cur).map(|c| (c, lo, hi)))
        } else if second.is_empty() {
            let m = Missing::Neutral("no frames in the last second".into());
            (Err(m.clone()), Err(m))
        } else {
            let sum: u64 = second.iter().map(|&u| u64::from(u)).sum();
            let n = second.len() as f64;
            let (lo, hi) = min_max_ms(&second).unwrap_or((0.0, 0.0));
            let avg = sum as f64 / n / 1000.0;
            (
                positive(1e6 * n / sum as f64),
                positive(avg).map(|a| (a, lo, hi)),
            )
        };
        let lows = cached_lows(&mut self.lows, g.pid, approx, now, ring);
        let gpu_time = match g.gpu {
            Some((active, frame)) if active > 0 && frame > 0 => {
                Ok((f64::from(active) / 1000.0, f64::from(frame) / 1000.0))
            }
            Some(_) => neutral("not available in this game"),
            None => neutral("needs RTSS shared memory 2.21+"),
        };
        let bound = match (&gpu_time, &frametime) {
            (Ok((active, _)), Ok((avg, _, _))) if *avg > 0.0 => Ok(if active / avg >= GPU_BOUND {
                "GPU"
            } else {
                "CPU"
            }),
            // O estado de quem falta (neutro num travamento curto) passa adiante.
            (Err(e), _) | (_, Err(e)) => Err(e.clone()),
            _ => fail("no frametime"),
        };
        Game {
            pid: g.pid,
            name: g.name.clone(),
            api: api_name(g.api),
            resolution: match g.res {
                Some((x, y)) if x > 0 && y > 0 => Ok((x, y)),
                _ => fail("resolution not reported by RTSS"),
            },
            approx,
            fps,
            frametime,
            lows,
            gpu_time,
            bound,
        }
    }
}

fn cached_lows(
    cache: &mut LowsCache,
    pid: u32,
    approx: bool,
    now: Instant,
    ring: Option<&Ring>,
) -> Metric<(f64, f64)> {
    if let Some((t, p, a, v)) = &*cache
        && *p == pid
        && *a == approx
        && now.duration_since(*t) < LOWS_EVERY
    {
        return v.clone();
    }
    let v = ring
        .and_then(|r| lows(r.frames.iter().map(|(_, us)| *us)))
        .map_or_else(|| neutral("waiting for samples"), Ok);
    *cache = Some((now, pid, approx, v.clone()));
    v
}

fn positive(v: f64) -> Metric<f64> {
    if v.is_finite() && v > 0.0 {
        Ok(v)
    } else {
        fail("no frames in the last second")
    }
}

fn min_max_ms(us: &[u32]) -> Option<(f64, f64)> {
    let lo = us.iter().min()?;
    let hi = us.iter().max()?;
    Some((f64::from(*lo) / 1000.0, f64::from(*hi) / 1000.0))
}

/// Inicia o RTSS pelo caminho do registro (visão de 32 bits; o RTSS é um programa de 32 bits).
fn start_rtss() -> Result<(), String> {
    let path = win::reg_sz_32(RTSS_KEY, "InstallPath").ok_or("RTSS not installed")?;
    let path = std::path::PathBuf::from(path);
    if !path.is_absolute() || !path.is_file() {
        return Err(format!("invalid RTSS InstallPath {}", path.display()));
    }
    let dir = path.parent().ok_or("RTSS InstallPath has no folder")?;
    // Diretório de trabalho = pasta do RTSS (não o do wintop, que pode ser gravável pelo
    // usuário) e sem herdar a entrada e a saída do wintop.
    std::process::Command::new(&path)
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(drop)
        .map_err(|e| format!("could not start {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(index: u32, name: &str, api: u32, fps: f64, age: Option<u32>) -> App {
        App {
            index,
            pid: index + 100,
            name: name.into(),
            api,
            fps,
            age_ms: age,
            ..App::default()
        }
    }

    #[test]
    fn idade_sem_underflow() {
        assert_eq!(age_ms(1000, 0), None);
        assert_eq!(age_ms(1000, 900), Some(100));
        assert_eq!(age_ms(1000, 1001), Some(0));
        assert_eq!(age_ms(5, u32::MAX - 4), Some(10));
    }

    #[test]
    fn escolha_do_jogo() {
        let apps = vec![
            app(1, "spotifast.exe", 1, 100.0, Some(10)),
            app(2, "deadlock.exe", 6, 33.0, Some(40)),
            app(3, "outro.exe", 7, 120.0, Some(9000)),
            app(4, "semapi.exe", 0, 60.0, Some(10)),
        ];
        assert_eq!(select_game(&apps, Some(101)).map(|a| a.index), Some(2));
        assert_eq!(select_game(&apps, None).map(|a| a.index), Some(2));
        assert_eq!(select_game(&apps[2..], None).map(|a| a.index), None);
    }

    #[test]
    fn lows_basicos() {
        assert_eq!(lows([16_000u32; 50].into_iter()), None);
        assert_eq!(lows([0u32; 200].into_iter()), None);
        let mut s = vec![10_000u32; 990];
        s.extend([50_000u32; 10]);
        let (l1, l01) = lows(s.into_iter()).expect("lows");
        assert!((l1 - 20.0).abs() < 1e-9);
        assert!((l01 - 20.0).abs() < 1e-9);
    }

    #[test]
    fn registros_partidos_entre_mensagens() {
        let mut rings = HashMap::new();
        let mut carry = Vec::new();
        let mut rec = 7u32.to_le_bytes().to_vec();
        rec.extend(16_000u32.to_le_bytes());
        feed(&mut rings, &mut carry, &rec[..5], false); // ERROR_MORE_DATA no meio
        feed(&mut rings, &mut carry, &rec[5..], true);
        let got: Vec<u32> = rings[&7].frames.iter().map(|(_, us)| *us).collect();
        assert_eq!(got, vec![16_000]);
        feed(&mut rings, &mut carry, &rec[..3], true); // malformada: descartada
        assert!(carry.is_empty());
        // Índice fora da Shared Memory é ignorado.
        let mut bad = 9999u32.to_le_bytes().to_vec();
        bad.extend(1u32.to_le_bytes());
        feed(&mut rings, &mut carry, &bad, true);
        assert!(!rings.contains_key(&9999));
    }

    #[test]
    fn frametime_zero_ignorado() {
        let mut rings = HashMap::new();
        let mut carry = Vec::new();
        let mut rec = 3u32.to_le_bytes().to_vec();
        rec.extend(0u32.to_le_bytes());
        feed(&mut rings, &mut carry, &rec, true);
        assert!(!rings.contains_key(&3));
    }

    #[test]
    fn janela_e_ultimo_segundo() {
        let mut r = Ring::default();
        let t0 = Instant::now();
        r.push(t0, 10_000);
        r.push(t0 + Duration::from_secs(30), 20_000);
        r.push(t0 + Duration::from_secs(61), 30_000);
        let now = t0 + Duration::from_secs(61);
        r.prune(now);
        assert_eq!(r.frames.len(), 2); // o de t0 saiu da janela de 60 s
        assert_eq!(r.last_second(now).collect::<Vec<_>>(), vec![30_000]);
        assert!(r.recent(now, Duration::from_secs(2)));
        assert!(!r.recent(now + Duration::from_secs(3), Duration::from_secs(2)));
    }
}
