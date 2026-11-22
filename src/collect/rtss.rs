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

use std::collections::{HashMap, HashSet, VecDeque};
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    ERROR_IO_PENDING, ERROR_MORE_DATA, ERROR_NO_DATA, ERROR_OPERATION_ABORTED,
    ERROR_PIPE_CONNECTED, HANDLE, HLOCAL, LocalFree, WAIT_OBJECT_0,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
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
    CreateEventW, ResetEvent, SetEvent, WaitForMultipleObjects, WaitForSingleObject,
};
use windows::core::PCWSTR;

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

/// Janela em que um jogo exato (pipe) continua exato sem frames: acima de `ACTIVE_MS` (que usa
/// o relógio do RTSS, de 15,6 ms), para um travamento de ~2 s não piscar como "approx.".
const EXACT_GRACE: Duration = Duration::from_millis(ACTIVE_MS as u64 + 500);
/// Pausa sem frames a partir da qual o FPS passa a contar o tempo parado.
const FREEZE_MIN_US: u64 = 50_000;
/// Período mais longo entre amostras da thread do RTSS (sem jogo).
const RTSS_PERIOD_MAX: Duration = Duration::from_secs(1);
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
        // O RTSS informa D3D10 para jogos D3D11 (Deadlock: 0x6, e o jogo carrega D3D11). Os
        // dois passam pelo DXGI e confirmar a API exigiria ler os módulos do jogo (proibido
        // pelo anticheat): nunca afirmar a errada.
        0x6 | 0x7 => "D3D10/11",
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
    // SAFETY: sem pré-condições; lido a cada entrada, depois de `t1` (ver `age_ms`).
    read_apps_at(v, || unsafe { GetTickCount() })
}

/// Como `read_apps`, com o relógio dado (o fuzz usa um instante fixo: sementes reproduzíveis).
fn read_apps_at(
    v: &SharedView,
    tick: impl Fn() -> u32,
) -> Result<(Vec<App>, Option<u32>, u32), String> {
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
        let now = tick();
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

/// Nome de executável na lista de ignorados (comparação sem diferenciar maiúsculas).
fn is_ignored(list: &[String], name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    list.contains(&n)
}

/// Jogo atual: ativo, com API 3D e fora da lista de ignorados; o primeiro plano desempata,
/// senão o de maior FPS.
fn select_game<'a>(apps: &'a [App], fg: Option<u32>, ignored: &[String]) -> Option<&'a App> {
    let candidates: Vec<&App> = apps
        .iter()
        .filter(|a| a.api != 0 && a.age_ms.is_some_and(|m| m < ACTIVE_MS))
        .filter(|a| !is_ignored(ignored, &a.name))
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
        self.push_capped(at, us, MAX_FRAMES);
    }

    /// O pipe aceita escrita de processos sem elevação: um cliente inundando não cresce a
    /// memória sem limite (descarta os mais antigos).
    fn push_capped(&mut self, at: Instant, us: u32, cap: usize) {
        while self.frames.len() >= cap.max(1) {
            self.frames.pop_front();
        }
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
        // Depois de um pico (menu sem limite de FPS), devolve a memória.
        if self.frames.capacity() > 4 * self.frames.len().max(1024) {
            self.frames.shrink_to(self.frames.len() * 2);
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
///
/// `known`: índices de apps (fora dos ignorados) na última leitura da Shared Memory. Um índice
/// ainda desconhecido (jogo que acabou de abrir, antes da próxima amostra) entra com teto
/// pequeno, em no máximo `MAX_UNKNOWN_RINGS` filas: um cliente local inundando o pipe com
/// índices inventados não faz a memória crescer.
fn feed(
    rings: &mut HashMap<u32, Ring>,
    known: &HashSet<u32>,
    ignored: &HashSet<u32>,
    carry: &mut Vec<u8>,
    data: &[u8],
    complete: bool,
) {
    carry.extend_from_slice(data);
    let whole = carry.len() / 8 * 8;
    let now = Instant::now();
    let mut unknown = rings.keys().filter(|i| !known.contains(i)).count();
    for rec in carry[..whole].as_chunks::<8>().0 {
        let app = u32::from_le_bytes([rec[0], rec[1], rec[2], rec[3]]);
        let us = u32::from_le_bytes([rec[4], rec[5], rec[6], rec[7]]);
        // Índice fora da Shared Memory ou frametime 0 (sem delta): registro ignorado.
        if (app as usize) >= MAX_APPS || us == 0 || ignored.contains(&app) {
            continue;
        }
        if known.contains(&app) {
            rings.entry(app).or_default().push(now, us);
        } else if let Some(r) = rings.get_mut(&app) {
            r.push_capped(now, us, UNKNOWN_FRAMES);
        } else if unknown < MAX_UNKNOWN_RINGS {
            unknown += 1;
            rings
                .entry(app)
                .or_default()
                .push_capped(now, us, UNKNOWN_FRAMES);
        }
    }
    carry.drain(..whole);
    if complete {
        carry.clear();
    }
}

// --- Pipe ---

const INSTANCES: usize = 8;
/// Buffer de leitura por instância: 512 registros de 8 bytes por leitura (mensagens maiores
/// chegam em partes, com ERROR_MORE_DATA).
const BUF: usize = 4 * 1024;
/// Frames guardados por app: 1000 FPS × 60 s.
const MAX_FRAMES: usize = 60_000;
/// Conclusões de leitura atendidas por período: acima disso o pipe espera o próximo período
/// (um cliente inundando não prende a thread num laço).
const MAX_SIGNALS_PER_PUMP: u32 = 1024;
/// Índices ainda fora da Shared Memory: quantos e quantos frames cada (4 s a 1000 FPS).
const MAX_UNKNOWN_RINGS: usize = 8;
/// Apps com a janela completa de frames (os 3D mais recentes da Shared Memory).
const MAX_KNOWN: usize = 8;
const UNKNOWN_FRAMES: usize = 4096;

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
    fn create(name: &[u16], first: bool, sa: &SECURITY_ATTRIBUTES) -> Result<Self, String> {
        let mut mode: FILE_FLAGS_AND_ATTRIBUTES = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
        if first {
            mode |= FILE_FLAG_FIRST_PIPE_INSTANCE; // falha se outro servidor já existe
        }
        // SAFETY: atributos válidos durante a chamada.
        let pipe = unsafe {
            CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
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
    fn on_signal(
        &mut self,
        rings: &mut HashMap<u32, Ring>,
        known: &HashSet<u32>,
        ignored: &HashSet<u32>,
    ) {
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
                        feed(
                            rings,
                            known,
                            ignored,
                            &mut self.carry,
                            &self.buf[..got],
                            true,
                        );
                        self.start_read();
                    }
                    Err(e) if e.code() == ERROR_MORE_DATA.to_hresult() => {
                        feed(
                            rings,
                            known,
                            ignored,
                            &mut self.carry,
                            &self.buf[..got],
                            false,
                        );
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

/// As 8 instâncias do pipe, ou erro. Todas ou nenhuma: uma vaga livre deixaria outro processo
/// do usuário criar uma instância e receber conexões do hook (GW inclui CREATE_PIPE_INSTANCE).
/// Nome do pipe que o hook do RTSS procura.
const PIPE_NAME: &str = r"\\.\pipe\RTSS_Frametime";

fn create_pipe() -> Result<Vec<Instance>, String> {
    create_pipe_named(PIPE_NAME)
}

fn create_pipe_named(name: &str) -> Result<Vec<Instance>, String> {
    let name = wide(name);
    let sid = crate::win::user_sid().ok_or("could not read the user SID")?;
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
        .map(|i| Instance::create(&name, i == 0, &sa))
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

/// PIDs com o hook do RTSS agora (leitura direta da Shared Memory, sem esperar a thread do
/// RTSS): a última checagem antes de abrir um processo sob demanda.
pub fn hooked_now() -> Result<Vec<u32>, String> {
    let v = SharedView::open(SHM_NAME).ok_or("RTSS shared memory unavailable")?;
    let (apps, _, _) = read_apps(&v)?;
    Ok(apps.iter().map(|a| a.pid).collect())
}

// --- Coletor ---

/// `Fps` com a mesma falha em tudo (pânico ou início impossível da thread).
pub fn failed(cause: &str) -> Fps {
    let m = Missing::Fail(cause.to_string());
    Fps {
        rtss: Err(m.clone()),
        pipe: Err(m.clone()),
        game: Err(m),
        hooked: Vec::new(),
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
    /// Índices dos apps (fora dos ignorados) na última amostra: os que o pipe guarda inteiros.
    known: HashSet<u32>,
    /// Índices de apps ignorados (navegadores, overlays): os frames deles são descartados.
    ignored: HashSet<u32>,
    /// Executáveis (minúsculos) que nunca viram o jogo (`config.toml`, `[rtss] ignored`).
    ignored_names: Vec<String>,
    /// Amostras aproximadas da Shared Memory por PID.
    approx: HashMap<u32, Ring>,
    lows: LowsCache,
    /// PID e momento da publicação anterior do jogo (para o pior frame desde então).
    published: Option<(u32, Instant)>,
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
            known: HashSet::new(),
            ignored: HashSet::new(),
            ignored_names: crate::config::DEFAULT_RTSS_IGNORED
                .iter()
                .map(ToString::to_string)
                .collect(),
            approx: HashMap::new(),
            lows: None,
            published: None,
        }
    }

    /// Troca a lista de ignorados (do `config.toml`).
    pub fn with_ignored(mut self, list: Vec<String>) -> Self {
        self.ignored_names = list;
        self
    }

    /// Como `with_ignored`, com o coletor rodando: a próxima amostra refaz os índices
    /// conhecidos e ignorados com a lista nova.
    pub fn set_ignored(&mut self, list: Vec<String>) {
        self.ignored_names = list;
    }

    /// Atende o pipe até `deadline` (conclusões de conexão e leitura). Volta na hora se `stop`
    /// (evento manual) for sinalizado; devolve `true` nesse caso.
    pub fn pump(&mut self, deadline: Instant, stop: HANDLE) -> bool {
        let mut handled = 0u32;
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
            if handled >= MAX_SIGNALS_PER_PUMP {
                // SAFETY: evento válido.
                return unsafe { WaitForSingleObject(stop, timeout) } == WAIT_OBJECT_0;
            }
            // O evento de parada vem primeiro (índice 0); depois os das instâncias armadas,
            // começando por uma diferente a cada volta (justiça entre clientes).
            let mut events = [HANDLE::default(); INSTANCES + 1];
            let mut armed = [0usize; INSTANCES];
            events[0] = stop;
            let mut n = 0;
            for i in 0..instances.len() {
                let k = (i + self.turn) % instances.len();
                if instances[k].pending != Pending::None {
                    armed[n] = k;
                    events[n + 1] = h(&instances[k].event);
                    n += 1;
                }
            }
            self.turn = self.turn.wrapping_add(1);
            // SAFETY: eventos válidos (no máximo 9).
            let r = unsafe { WaitForMultipleObjects(&events[..=n], false, timeout) };
            let idx = r.0.wrapping_sub(WAIT_OBJECT_0.0) as usize;
            if idx == 0 {
                return true;
            }
            match armed[..n].get(idx - 1) {
                Some(&k) => {
                    instances[k].on_signal(&mut self.exact, &self.known, &self.ignored);
                    handled += 1;
                }
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
        // Sem Shared Memory válida, nenhum índice é "conhecido" (o teto vale para todos).
        self.known.clear();
        self.ignored.clear();
        self.published = None;
    }

    /// Uma amostra: estado do RTSS e do jogo.
    pub fn sample(&mut self) -> Fps {
        if let Ok(list) = &mut self.pipe {
            for inst in list.iter_mut().filter(|i| i.pending == Pending::None) {
                inst.start_connect();
            }
        }
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
                hooked: Vec::new(),
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
                    hooked: Vec::new(),
                };
            }
            None => unreachable!("view aberta em open_view"),
        };
        let rtss = Ok(format!("shared memory {}.{}", ver >> 16, ver & 0xFFFF));
        let now = Instant::now();
        // Só os índices de apps acompanhados (os ignorados, como navegadores, não guardam 60 s
        // de frames).
        let (ign, ok): (Vec<&App>, Vec<&App>) = apps
            .iter()
            .partition(|a| is_ignored(&self.ignored_names, &a.name));
        // Janela completa (60 s) só para os MAX_KNOWN apps 3D com frames mais recentes: a
        // tabela do RTSS guarda apps fechados e, com uma Shared Memory falsa, até 256 entradas
        // (256 × 60 mil frames). Um jogo travado continua entre os mais recentes.
        let mut active: Vec<&App> = ok.iter().copied().filter(|a| a.api != 0).collect();
        // O app em primeiro plano sempre entra (pausado, ele envelhece mais que os outros).
        active.sort_by_key(|a| (Some(a.pid) != fg, a.age_ms.unwrap_or(u32::MAX)));
        active.truncate(MAX_KNOWN);
        self.known = active.iter().map(|a| a.index).collect();
        self.ignored = ign.iter().map(|a| a.index).collect();
        // Índice reaproveitado por outro app: os frames antigos não valem. Ring sem dono
        // (frames que chegaram antes desta amostra) fica com o app atual do índice.
        for a in &active {
            let ring = self.exact.entry(a.index).or_default();
            if ring.pid.is_some_and(|p| p != a.pid) {
                ring.frames.clear();
            }
            ring.pid = Some(a.pid);
        }
        self.exact.retain(|i, _| self.known.contains(i));
        self.approx
            .retain(|pid, _| apps.iter().any(|a| a.pid == *pid));
        for r in self.exact.values_mut().chain(self.approx.values_mut()) {
            r.prune(now);
        }
        let game = match select_game(&apps, fg, &self.ignored_names) {
            None => neutral("no 3D game in focus"),
            Some(g) => Ok(self.game(g, now)),
        };
        let hooked = apps.iter().map(|a| a.pid).collect();
        Fps {
            rtss,
            pipe,
            game,
            hooked,
        }
    }

    fn game(&mut self, g: &App, now: Instant) -> Game {
        // Exato enquanto o pipe trouxe frames na janela de atividade (um travamento de 1-2 s
        // não vira "approx." nem perde os lows exatos).
        let exact_recent = self
            .exact
            .get(&g.index)
            .is_some_and(|r| r.recent(now, EXACT_GRACE));
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
            // Congelamento em andamento: o tempo desde o último frame conta no FPS (senão um
            // travamento de 900 ms mostraria o FPS do frame anterior a ele).
            let gap = ring
                .and_then(|r| r.frames.back())
                .map_or(0, |(t, _)| now.duration_since(*t).as_micros() as u64);
            let stalled = gap > FREEZE_MIN_US && gap as f64 > 2.0 * avg * 1000.0;
            let span = sum + if stalled { gap } else { 0 };
            (
                positive(1e6 * n / span as f64),
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
            // Os dois tempos da mesma linha do RTSS (o frametime médio do pipe tem 1 s de
            // inércia e inverteria o veredito numa troca de cena).
            (Ok((active, frame)), Ok(_)) if *frame > 0.0 => Ok(if active / frame >= GPU_BOUND {
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
                Some(_) => fail("resolution not reported by RTSS"),
                None => neutral("needs RTSS shared memory 2.20+"),
            },
            approx,
            fps,
            frametime,
            lows,
            gpu_time,
            bound,
            peak_ms: {
                // Só o que chegou desde a publicação anterior deste jogo (outro jogo ou a 1ª
                // publicação: no máximo o último período, não 60 s de histórico).
                let floor = now.checked_sub(RTSS_PERIOD_MAX).unwrap_or(now);
                let since = match self.published.replace((g.pid, now)) {
                    Some((pid, t)) if pid == g.pid => Some(t.max(floor)),
                    _ => Some(floor),
                };
                ring.and_then(|r| {
                    r.frames
                        .iter()
                        .rev()
                        .take_while(|(t, _)| since.is_none_or(|s| *t > s))
                        .map(|(_, us)| *us)
                        .max()
                })
                .map(|us| f64::from(us) / 1000.0)
            },
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
        neutral("no frames in the last second")
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
    if !win::is_local_absolute(&path) || !path.is_file() {
        return Err(format!("invalid RTSS InstallPath {}", path.display()));
    }
    let dir = path.parent().ok_or("RTSS InstallPath has no folder")?;
    // O RTSS roda elevado (filho do wintop): só de uma pasta que ninguém sem elevação altera.
    crate::install::trusted_file(&path)?;
    // Diretório de trabalho = pasta do RTSS (não o do wintop, que pode ser gravável pelo
    // usuário) e sem herdar a entrada e a saída do wintop. PATH só com as pastas do sistema: o
    // herdado vem do ambiente do usuário (HKCU\Environment), e uma DLL procurada no PATH viraria
    // código do usuário rodando elevado.
    // Processo próprio e fora do grupo de console do wintop (Ctrl+C no wintop não o atinge).
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    let sys = win::system_dir().ok_or("System32 not found")?;
    let windir = win::windows_dir().ok_or("Windows folder not found")?;
    std::process::Command::new(&path)
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .current_dir(dir)
        .env("PATH", format!("{};{}", sys.display(), windir.display()))
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
        assert_eq!(
            select_game(&apps, Some(101), &defaults()).map(|a| a.index),
            Some(2)
        );
        assert_eq!(
            select_game(&apps, None, &defaults()).map(|a| a.index),
            Some(2)
        );
        assert_eq!(
            select_game(&apps[2..], None, &defaults()).map(|a| a.index),
            None
        );
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

    /// Shared Memory sintética com o layout do header (v2.21 por padrão).
    fn shm(ver: u32, esize: usize, apps: &[(u32, &[u8], u32)]) -> Vec<u8> {
        const ARR: usize = 0x100;
        let mut b = vec![0u8; ARR + esize * apps.len().max(1)];
        let put = |b: &mut Vec<u8>, o: usize, v: u32| b[o..o + 4].copy_from_slice(&v.to_le_bytes());
        put(&mut b, 0, RTSS_SIGNATURE);
        put(&mut b, OFF_VERSION, ver);
        put(&mut b, OFF_APP_ENTRY_SIZE, esize as u32);
        put(&mut b, OFF_APP_ARR_OFFSET, ARR as u32);
        put(&mut b, OFF_APP_ARR_SIZE, apps.len() as u32);
        put(
            &mut b,
            OFF_LAST_FOREGROUND_PID,
            apps.first().map_or(0, |a| a.0),
        );
        for (i, (pid, name, flags)) in apps.iter().enumerate() {
            let e = ARR + i * esize;
            put(&mut b, e + E_PID, *pid);
            b[e + E_NAME..e + E_NAME + name.len()].copy_from_slice(name);
            put(&mut b, e + E_FLAGS, *flags);
            put(&mut b, e + E_FRAME_TIME, 6944);
            if esize >= E_RES_Y + 4 {
                put(&mut b, e + E_RES_X, 2560);
                put(&mut b, e + E_RES_Y, 1440);
            }
            if esize >= E_GPU_FRAME + 4 {
                put(&mut b, e + E_GPU_ACTIVE, 5200);
                put(&mut b, e + E_GPU_FRAME, 6800);
            }
        }
        b
    }

    const ENTRY_V221: usize = 12416;

    #[test]
    fn shared_memory_v2_21() {
        let b = shm(
            V2_21,
            ENTRY_V221,
            &[
                (4242, br"C:\Games\deadlock.exe", 0x6),
                (0, b"", 0), // entrada vazia: ignorada
                (1784, b"dwm.exe", 0x7),
            ],
        );
        let (apps, fg, ver) = read_apps(&SharedView::from_bytes(&b)).expect("leitura");
        assert_eq!(ver, V2_21);
        assert_eq!(fg, Some(4242));
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[0].name, "deadlock.exe");
        assert_eq!(apps[0].index, 0);
        assert_eq!(
            apps[1].index, 2,
            "o índice é a posição no array (= dwApp do pipe)"
        );
        assert_eq!(apps[0].res, Some((2560, 1440)));
        assert_eq!(apps[0].gpu, Some((5200, 6800)));
        assert_eq!(apps[0].frametime_us, 6944);
    }

    /// A Shared Memory é escrita por outro processo (e pode ser criada por um processo sem
    /// elevação no lugar do RTSS): qualquer conteúdo vira erro ou apps coerentes.
    #[test]
    fn fuzz_shared_memory() {
        // Período de FPS preenchido (agora - 500 ms .. agora, 72 frames): o cálculo do FPS e da
        // idade é exercitado, não só o caso "sem período".
        // Relógio fixo (o mesmo na leitura): a semente reproduz a falha em qualquer máquina.
        const NOW: u32 = 10_000;
        let timed = |mut b: Vec<u8>, esize: usize, n: usize| {
            for i in 0..n {
                let e = 0x100 + i * esize;
                for (o, v) in [(E_TIME0, NOW - 500), (E_TIME1, NOW), (E_FRAMES, 72)] {
                    b[e + o..e + o + 4].copy_from_slice(&v.to_le_bytes());
                }
            }
            b
        };
        let seeds = [
            timed(
                shm(
                    V2_21,
                    ENTRY_V221,
                    &[(4242, br"C:\g\a.exe", 6), (7, b"b.exe", 7)],
                ),
                ENTRY_V221,
                2,
            ),
            timed(
                shm(0x0002_000F, E_FRAME_TIME + 4, &[(10, b"old.exe", 5)]),
                E_FRAME_TIME + 4,
                1,
            ),
            timed(
                shm(V2_20, E_RES_Y + 4, &[(11, b"mid.exe", 5), (0, b"", 0)]),
                E_RES_Y + 4,
                2,
            ),
            // Entradas pequenas e muitas (a maior parte dos bytes é cabeçalho e campos lidos).
            timed(
                shm(V2_21, E_FRAME_TIME + 4, &[(1u32, &b"x.exe"[..], 6u32); 8]),
                E_FRAME_TIME + 4,
                8,
            ),
        ];
        let ignored = defaults();
        crate::fuzz::run("shared_memory", &seeds, 32 * 1024, |r, b| {
            let mut b = b.to_vec();
            // Metade das entradas com a assinatura certa: as mutações chegam ao resto.
            if b.len() >= 4 && r.chance(2) {
                b[..4].copy_from_slice(&RTSS_SIGNATURE.to_le_bytes());
            }
            let Ok((apps, fg, ver)) = read_apps_at(&SharedView::from_bytes(&b), || NOW) else {
                return;
            };
            assert_eq!(ver >> 16, 2);
            assert!(ver >= V2_16 || fg.is_none());
            assert!(apps.len() <= MAX_APPS);
            for a in &apps {
                assert!(a.pid != 0 && (a.index as usize) < MAX_APPS);
                assert!(a.fps.is_finite() && a.fps >= 0.0);
                // O nome vem de um processo qualquer: nunca chega com controles ao terminal.
                assert!(!a.name.contains('\\') && !a.name.chars().any(char::is_control));
                assert!(ver >= V2_20 || a.res.is_none());
                assert!(ver >= V2_21 || a.gpu.is_none());
            }
            if let Some(g) = select_game(&apps, fg, &ignored) {
                assert!(g.api != 0 && g.age_ms.is_some_and(|m| m < ACTIVE_MS));
                assert!(!is_ignored(&ignored, &g.name));
            }
        });
    }

    /// O pipe aceita escrita de processos sem elevação: mensagens quaisquer, partidas em
    /// pedaços quaisquer, mantêm os tetos (filas desconhecidas, frames, sobra < 8 bytes).
    #[test]
    fn fuzz_pipe() {
        let rec = |app: u32, us: u32| {
            let mut v = app.to_le_bytes().to_vec();
            v.extend(us.to_le_bytes());
            v
        };
        let mut seed = rec(0, 6944);
        seed.extend(rec(3, 16_000));
        seed.extend(rec(200, 1));
        let known: HashSet<u32> = [0, 1].into();
        let ignored: HashSet<u32> = [1].into();
        let mut rings = HashMap::new();
        let mut carry = Vec::new();
        crate::fuzz::run("pipe", &[seed], 512, |r, b| {
            // Recomeça de vez em quando: as filas desconhecidas voltam a ser criadas (o ramo
            // do teto não fica sempre cheio) e a conferência abaixo não varre 90 mil frames.
            if r.chance(64) {
                rings.clear();
                carry.clear();
            }
            let complete = r.chance(2);
            feed(&mut rings, &known, &ignored, &mut carry, b, complete);
            assert!(carry.len() < 8);
            assert!(!complete || carry.is_empty());
            let unknown = rings.keys().filter(|i| !known.contains(i)).count();
            assert!(unknown <= MAX_UNKNOWN_RINGS);
            for (i, ring) in &rings {
                assert!((*i as usize) < MAX_APPS && !ignored.contains(i));
                let cap = if known.contains(i) {
                    MAX_FRAMES
                } else {
                    UNKNOWN_FRAMES
                };
                assert!(ring.frames.len() <= cap);
                assert!(ring.frames.iter().all(|&(_, us)| us > 0));
            }
        });
    }

    #[test]
    fn fuzz_lows() {
        // Até ~1100 amostras: passa pelas duas regras do 0,1% (abaixo e acima de 1000).
        crate::fuzz::run("lows", &[], 4400, |_, b| {
            let s = b.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c));
            if let Some((l1, l01)) = lows(s) {
                assert!(l1.is_finite() && l01.is_finite() && l1 > 0.0 && l01 > 0.0);
                assert!(l01 <= l1 + 1e-9, "0,1% low acima do 1% low");
            }
        });
    }

    #[test]
    fn shared_memory_antiga_sem_campos_novos() {
        // v2.15: sem primeiro plano, resolução e tempo de GPU; entrada no tamanho antigo.
        let b = shm(0x0002_000F, E_FRAME_TIME + 4, &[(10, b"old.exe", 0x5)]);
        let (apps, fg, _) = read_apps(&SharedView::from_bytes(&b)).expect("leitura");
        assert_eq!(fg, None);
        assert_eq!(apps[0].res, None);
        assert_eq!(apps[0].gpu, None);
    }

    #[test]
    fn shared_memory_invalida_e_erro() {
        let ok = shm(V2_21, ENTRY_V221, &[(1, b"a.exe", 1)]);
        let read = |b: &[u8]| read_apps(&SharedView::from_bytes(b)).map(|_| ());
        // RTSS fechando.
        let mut dead = ok.clone();
        dead[..4].copy_from_slice(&RTSS_DEAD.to_le_bytes());
        assert_eq!(read(&dead), Err("RTSS is shutting down".into()));
        // Assinatura errada e versão 1.x.
        let mut sig = ok.clone();
        sig[..4].copy_from_slice(&0x1234u32.to_le_bytes());
        assert!(read(&sig).is_err());
        let mut v1 = ok.clone();
        v1[OFF_VERSION..OFF_VERSION + 4].copy_from_slice(&0x0001_0005u32.to_le_bytes());
        assert!(read(&v1).is_err());
        // Entrada menor que o mínimo.
        let mut small = ok.clone();
        small[OFF_APP_ENTRY_SIZE..OFF_APP_ENTRY_SIZE + 4].copy_from_slice(&16u32.to_le_bytes());
        assert!(read(&small).is_err());
        // Array além do fim da região (e offset que estoura na multiplicação).
        let mut far = ok.clone();
        far[OFF_APP_ARR_OFFSET..OFF_APP_ARR_OFFSET + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(read(&far).is_err());
        let mut many = ok.clone();
        many[OFF_APP_ARR_SIZE..OFF_APP_ARR_SIZE + 4].copy_from_slice(&200u32.to_le_bytes());
        assert!(read(&many).is_err());
        // Header cortado.
        assert!(read(&ok[..10]).is_err());
    }

    #[test]
    fn nome_com_controles_nao_chega_ao_terminal() {
        let b = shm(V2_21, ENTRY_V221, &[(1, b"ev\x1b[2Jil\x9b.exe", 1)]);
        let (apps, _, _) = read_apps(&SharedView::from_bytes(&b)).expect("leitura");
        assert!(
            !apps[0].name.chars().any(char::is_control),
            "{:?}",
            apps[0].name
        );
    }

    /// Servidor do pipe real com um nome de teste e clientes falsos (sem RTSS nem jogo): os
    /// registros chegam inteiros, inclusive partidos em mensagens; a 9ª conexão é recusada
    /// (as 8 instâncias são todas nossas); o evento de parada interrompe a espera na hora.
    #[test]
    fn servidor_do_pipe_com_clientes_falsos() {
        use std::io::Write;
        use std::os::windows::io::FromRawHandle;
        let name = format!(r"\\.\pipe\wintop_test_{}", std::process::id());
        let mut c = RtssCollector::new(false, false);
        c.pipe = create_pipe_named(&name).map_err(Some);
        assert!(c.pipe.is_ok(), "{:?}", c.pipe.as_ref().err());
        // SAFETY: evento sem nome; o handle passa a ter dono na hora.
        let ev = unsafe {
            OwnedHandle::from_raw_handle(
                windows::Win32::System::Threading::CreateEventW(None, true, false, None)
                    .expect("evento")
                    .0,
            )
        };
        let stop = HANDLE(ev.as_raw_handle());
        let rec = |app: u32, us: u32| [app.to_le_bytes(), us.to_le_bytes()].concat();
        let open = || std::fs::OpenOptions::new().write(true).open(&name);
        let mut a = open().expect("cliente 1");
        let mut b = open().expect("cliente 2");
        // 100 frames do app 3 num cliente; 50 do app 5 em outro, numa mensagem só.
        for _ in 0..100 {
            a.write_all(&rec(3, 7000)).expect("escrita");
        }
        let many: Vec<u8> = (0..50).flat_map(|_| rec(5, 16_000)).collect();
        b.write_all(&many).expect("escrita");
        // 8 instâncias: com 2 em uso, mais 6 conectam e a 9ª é recusada.
        let extra: Vec<_> = (0..6).map(|_| open().expect("cliente extra")).collect();
        assert!(open().is_err(), "9ª conexão recusada");
        c.pump(Instant::now() + Duration::from_millis(300), stop);
        assert_eq!(c.exact.get(&3).map(|r| r.frames.len()), Some(100));
        assert_eq!(c.exact.get(&5).map(|r| r.frames.len()), Some(50));
        drop(extra);
        // Parada: o pump volta na hora mesmo com prazo longo.
        // SAFETY: evento válido.
        unsafe {
            let _ = windows::Win32::System::Threading::SetEvent(stop);
        }
        let t = Instant::now();
        assert!(c.pump(Instant::now() + Duration::from_secs(10), stop));
        assert!(
            t.elapsed() < Duration::from_millis(200),
            "{:?}",
            t.elapsed()
        );
    }

    fn defaults() -> Vec<String> {
        crate::config::DEFAULT_RTSS_IGNORED
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    fn all_known() -> HashSet<u32> {
        (0..MAX_APPS as u32).collect()
    }

    #[test]
    fn indices_desconhecidos_tem_teto() {
        let mut rings = HashMap::new();
        let mut carry = Vec::new();
        let known: HashSet<u32> = [3].into_iter().collect();
        // Um cliente inunda 200 índices inventados com 10 mil frames cada.
        let mut data = Vec::new();
        for app in 0..200u32 {
            for _ in 0..50 {
                data.extend(app.to_le_bytes());
                data.extend(1000u32.to_le_bytes());
            }
        }
        for _ in 0..200 {
            feed(&mut rings, &known, &HashSet::new(), &mut carry, &data, true);
        }
        let unknown: Vec<_> = rings.keys().filter(|i| !known.contains(i)).collect();
        assert!(unknown.len() <= MAX_UNKNOWN_RINGS, "{}", unknown.len());
        assert!(rings.values().all(|r| r.frames.len() <= MAX_FRAMES));
        assert!(
            rings
                .iter()
                .filter(|(i, _)| !known.contains(i))
                .all(|(_, r)| r.frames.len() <= UNKNOWN_FRAMES)
        );
        // O índice conhecido guarda tudo.
        assert_eq!(rings[&3].frames.len(), 200 * 50);
    }

    #[test]
    fn registros_partidos_entre_mensagens() {
        let mut rings = HashMap::new();
        let mut carry = Vec::new();
        let mut rec = 7u32.to_le_bytes().to_vec();
        rec.extend(16_000u32.to_le_bytes());
        feed(
            &mut rings,
            &all_known(),
            &HashSet::new(),
            &mut carry,
            &rec[..5],
            false,
        ); // ERROR_MORE_DATA no meio
        feed(
            &mut rings,
            &all_known(),
            &HashSet::new(),
            &mut carry,
            &rec[5..],
            true,
        );
        let got: Vec<u32> = rings[&7].frames.iter().map(|(_, us)| *us).collect();
        assert_eq!(got, vec![16_000]);
        feed(
            &mut rings,
            &all_known(),
            &HashSet::new(),
            &mut carry,
            &rec[..3],
            true,
        ); // malformada: descartada
        assert!(carry.is_empty());
        // Índice fora da Shared Memory é ignorado.
        let mut bad = 9999u32.to_le_bytes().to_vec();
        bad.extend(1u32.to_le_bytes());
        feed(
            &mut rings,
            &all_known(),
            &HashSet::new(),
            &mut carry,
            &bad,
            true,
        );
        assert!(!rings.contains_key(&9999));
    }

    #[test]
    fn frametime_zero_ignorado() {
        let mut rings = HashMap::new();
        let mut carry = Vec::new();
        let mut rec = 3u32.to_le_bytes().to_vec();
        rec.extend(0u32.to_le_bytes());
        feed(
            &mut rings,
            &all_known(),
            &HashSet::new(),
            &mut carry,
            &rec,
            true,
        );
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
        // Teto de frames: um cliente inundando não cresce sem limite.
        let mut big = Ring::default();
        for i in 0..(MAX_FRAMES + 10) {
            big.push(now, i as u32 + 1);
        }
        assert_eq!(big.frames.len(), MAX_FRAMES);
        assert_eq!(big.frames.front().map(|f| f.1), Some(11));
        assert!(r.recent(now, Duration::from_secs(2)));
        assert!(!r.recent(now + Duration::from_secs(3), Duration::from_secs(2)));
    }
}
