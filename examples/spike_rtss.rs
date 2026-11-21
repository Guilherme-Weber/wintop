//! Spike da Etapa 0: RTSS (Shared Memory v2.x e pipe de frametime).
//!
//! 1. Lê os aplicativos que o RTSS acompanha (`RTSSSharedMemoryV2`): FPS, frametime, API,
//!    resolução e tempos de GPU. Offsets gerados pelo compilador MSVC a partir do header
//!    oficial (`sizeof(RTSS_SHARED_MEMORY_APP_ENTRY)` = 12416 na v2.21); campos novos só são
//!    lidos se a versão e o tamanho da entrada os cobrirem.
//! 2. Escolhe o jogo: atividade recente + API 3D + fora da lista de ignorados; o app em primeiro
//!    plano desempata.
//! 3. Servidor `\\.\pipe\RTSS_Frametime` com 8 instâncias (o hook do RTSS está em vários
//!    processos ao mesmo tempo), DACL restrita, rótulo de integridade média, I/O overlapped e
//!    sem clientes remotos. Recebe `{dwApp, dwFrametime}` por frame.
//! 4. Fallback (jogo aberto antes do pipe): amostra o frametime da Shared Memory a cada 250 ms;
//!    os lows calculados dessas amostras são aproximados.
//!
//! `cargo run --example spike_rtss [segundos]`

#[path = "common/mod.rs"]
mod common;

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    ERROR_IO_PENDING, ERROR_MORE_DATA, ERROR_OPERATION_ABORTED, ERROR_PIPE_CONNECTED, HANDLE,
    HLOCAL, LocalFree, WAIT_OBJECT_0,
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
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId,
    PIPE_READMODE_MESSAGE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_MESSAGE, PIPE_WAIT,
};
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW, WaitForMultipleObjects,
};
use windows::core::{PCWSTR, PWSTR, w};

// RTSS_SHARED_MEMORY
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
/// Na Etapa 3 vira configuração.
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

fn api_name(flags: u32) -> &'static str {
    match flags & 0xFFFF {
        0 => "-",
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
        _ => "API ?",
    }
}

#[derive(Clone, Debug, Default)]
struct App {
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

/// Idade sem underflow: `now` é lido depois de `t1`; se o RTSS atualizou `t1` no meio
/// (diferença "negativa"), a idade é 0. `t1 == 0` = sem período registrado.
fn age_ms(now: u32, t1: u32) -> Option<u32> {
    if t1 == 0 {
        return None;
    }
    let d = now.wrapping_sub(t1);
    Some(if d > i32::MAX as u32 { 0 } else { d })
}

fn read_apps(v: &common::SharedView) -> Result<(Vec<App>, Option<u32>), String> {
    let oob = || "leitura fora dos limites".to_string();
    let sig = v.u32_at(0).ok_or_else(oob)?;
    if sig == RTSS_DEAD {
        return Err("RTSS encerrando (0xDEAD): desmapear e reconectar".into());
    }
    if sig != RTSS_SIGNATURE {
        return Err(format!("assinatura inválida {sig:#x}"));
    }
    let ver = v.u32_at(OFF_VERSION).ok_or_else(oob)?;
    if ver >> 16 != 2 {
        return Err(format!(
            "versão {}.{} não suportada",
            ver >> 16,
            ver & 0xFFFF
        ));
    }
    let esize = v.u32_at(OFF_APP_ENTRY_SIZE).ok_or_else(oob)? as usize;
    let arr = v.u32_at(OFF_APP_ARR_OFFSET).ok_or_else(oob)? as usize;
    let count = (v.u32_at(OFF_APP_ARR_SIZE).ok_or_else(oob)? as usize).min(MAX_APPS);
    if esize < E_FRAME_TIME + 4 {
        return Err(format!("entrada de {esize} B menor que o mínimo"));
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
        apps.push(App {
            pid,
            name,
            api: v.u32_at(e + E_FLAGS).ok_or_else(oob)? & 0xFFFF,
            fps,
            frametime_us: v.u32_at(e + E_FRAME_TIME).ok_or_else(oob)?,
            age_ms: age_ms(now, t1),
            res: if has_res {
                Some((
                    v.u32_at(e + E_RES_X).ok_or_else(oob)?,
                    v.u32_at(e + E_RES_Y).ok_or_else(oob)?,
                ))
            } else {
                None
            },
            gpu: if has_gpu {
                Some((
                    v.u32_at(e + E_GPU_ACTIVE).ok_or_else(oob)?,
                    v.u32_at(e + E_GPU_FRAME).ok_or_else(oob)?,
                ))
            } else {
                None
            },
        });
    }
    Ok((apps, fg))
}

/// Jogo atual: ativo (período de FPS recente), com API 3D e fora da lista de ignorados; o app
/// em primeiro plano desempata, senão o de maior FPS.
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

/// (1% low, 0,1% low) em FPS: média dos frametimes do 1% (0,1%) mais lento. Exige ao menos
/// 100 amostras; com menos de 1000, o 0,1% usa o pior frame.
fn lows(samples_us: &[u32]) -> Option<(f64, f64)> {
    if samples_us.len() < 100 {
        return None;
    }
    let mut s: Vec<u32> = samples_us.iter().copied().filter(|&u| u > 0).collect();
    s.sort_unstable_by(|a, b| b.cmp(a));
    let avg_fps = |k: usize| {
        let k = k.clamp(1, s.len());
        let avg = s[..k].iter().map(|&u| u64::from(u)).sum::<u64>() as f64 / k as f64;
        (avg > 0.0).then(|| 1e6 / avg)
    };
    Some((avg_fps(s.len() / 100)?, avg_fps(s.len() / 1000)?))
}

fn snapshot(title: &str) {
    println!("{title}");
    let Some(v) = common::SharedView::open("RTSSSharedMemoryV2") else {
        return println!("  RTSS: Shared Memory ausente (RTSS fechado?)");
    };
    match read_apps(&v) {
        Err(e) => println!("  RTSS: {e}"),
        Ok((apps, fg)) => {
            let game = select_game(&apps, fg).map(|g| g.pid);
            println!("  primeiro plano: {fg:?} | jogo escolhido: {game:?}");
            for a in &apps {
                println!(
                    "  {} PID {:>6} {:<26} {:<9} {:>6.1} FPS  {:>7.2} ms  idade {:>8}  res {:<11} GPU ativo/frame {}",
                    if Some(a.pid) == game { "*" } else { " " },
                    a.pid,
                    a.name,
                    api_name(a.api),
                    a.fps,
                    f64::from(a.frametime_us) / 1000.0,
                    a.age_ms
                        .map_or_else(|| "sem frames".into(), |m| format!("{m} ms")),
                    a.res
                        .map_or_else(|| "-".into(), |(x, y)| format!("{x}x{y}")),
                    a.gpu
                        .map_or_else(|| "-".into(), |(x, y)| format!("{x}/{y} µs")),
                );
            }
        }
    }
}

/// SID do usuário do token atual, em texto (S-1-5-21-...).
fn user_sid() -> Option<String> {
    let mut token = HANDLE::default();
    // SAFETY: token passa a ter dono logo abaixo.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) }.ok()?;
    let _token = common::own(token);
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

fn process_name(pid: u32) -> String {
    // SAFETY: handle passa a ter dono; buffer local.
    let Ok(h) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else {
        return "?".into();
    };
    let h = common::own(h);
    let mut buf = [0u16; 260];
    let mut len = buf.len() as u32;
    // SAFETY: buffer local com o tamanho informado.
    let r = unsafe {
        QueryFullProcessImageNameW(
            HANDLE(h.as_raw_handle()),
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &raw mut len,
        )
    };
    if r.is_err() {
        return "?".into();
    }
    let full = String::from_utf16_lossy(&buf[..len as usize]);
    full.rsplit('\\').next().unwrap_or(&full).to_string()
}

const INSTANCES: usize = 8;
const BUF: usize = 64 * 1024;

#[derive(PartialEq)]
enum Pending {
    None,
    Connect,
    Read,
}

/// Uma instância do pipe. `ov` e `buf` ficam em Box: o endereço não muda enquanto há I/O
/// pendente.
struct Instance {
    pipe: OwnedHandle,
    event: OwnedHandle,
    ov: Box<OVERLAPPED>,
    buf: Box<[u8]>,
    carry: Vec<u8>,
    pending: Pending,
    client: Option<u32>,
}

#[derive(Default)]
struct Stats {
    per_app: BTreeMap<u32, Vec<u32>>,
    second: BTreeMap<u32, (u64, u64, u32)>, // frames, soma µs, máx µs
    dropped_bytes: usize,
}

impl Stats {
    /// Registros completos de 8 bytes; sobra de mensagem incompleta fica em `carry`.
    fn feed(&mut self, carry: &mut Vec<u8>, data: &[u8], message_complete: bool) {
        carry.extend_from_slice(data);
        let whole = carry.len() / 8 * 8;
        let (records, _) = carry[..whole].as_chunks::<8>();
        for rec in records {
            let app = u32::from_le_bytes([rec[0], rec[1], rec[2], rec[3]]);
            let us = u32::from_le_bytes([rec[4], rec[5], rec[6], rec[7]]);
            self.per_app.entry(app).or_default().push(us);
            let s = self.second.entry(app).or_default();
            s.0 += 1;
            s.1 += u64::from(us);
            s.2 = s.2.max(us);
        }
        carry.drain(..whole);
        if message_complete && !carry.is_empty() {
            self.dropped_bytes += carry.len(); // mensagem malformada (não múltipla de 8)
            carry.clear();
        }
    }
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
        let pipe = common::own(pipe);
        // SAFETY: evento manual sem nome.
        let event = unsafe { CreateEventW(None, true, false, None) }
            .map_err(|e| format!("CreateEvent: {e}"))?;
        let event = common::own(event);
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
            client: None,
        })
    }

    fn start_connect(&mut self, stats: &mut Stats) {
        self.client = None;
        self.carry.clear();
        // SAFETY: `ov` (em Box) vive até a conclusão ou o cancelamento.
        match unsafe { ConnectNamedPipe(h(&self.pipe), Some(&raw mut *self.ov)) } {
            Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => self.pending = Pending::Connect,
            Ok(()) => self.on_connected(stats),
            Err(e) if e.code() == ERROR_PIPE_CONNECTED.to_hresult() => self.on_connected(stats),
            Err(e) => {
                println!("  ConnectNamedPipe: {e}");
                self.pending = Pending::None;
            }
        }
    }

    fn on_connected(&mut self, stats: &mut Stats) {
        let mut pid = 0u32;
        // SAFETY: pipe conectado.
        let _ = unsafe { GetNamedPipeClientProcessId(h(&self.pipe), &raw mut pid) };
        println!("  cliente conectado: PID {pid} ({})", process_name(pid));
        self.client = Some(pid);
        self.start_read(stats);
    }

    fn reconnect(&mut self, stats: &mut Stats) {
        if let Some(pid) = self.client {
            println!("  cliente PID {pid} desconectou");
        }
        // SAFETY: pipe válido; sem I/O pendente aqui.
        unsafe {
            let _ = DisconnectNamedPipe(h(&self.pipe));
        }
        self.start_connect(stats);
    }

    /// Emite leituras até uma ficar pendente; conclusões imediatas são processadas aqui.
    fn start_read(&mut self, stats: &mut Stats) {
        loop {
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
                Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {
                    self.pending = Pending::Read;
                    return;
                }
                Ok(()) => {
                    if !self.complete_read(stats) {
                        return;
                    }
                }
                Err(e) if e.code() == ERROR_MORE_DATA.to_hresult() => {
                    if !self.complete_read(stats) {
                        return;
                    }
                }
                Err(_) => {
                    self.pending = Pending::None;
                    return self.reconnect(stats);
                }
            }
        }
    }

    /// Resultado da leitura concluída; `false` se o cliente saiu (já reconectando).
    fn complete_read(&mut self, stats: &mut Stats) -> bool {
        let mut n = 0u32;
        // SAFETY: operação concluída sobre `ov`.
        let r =
            unsafe { GetOverlappedResult(h(&self.pipe), &raw const *self.ov, &raw mut n, false) };
        let data = &self.buf[..n as usize];
        match r {
            Ok(()) => stats.feed(&mut self.carry, data, true),
            Err(e) if e.code() == ERROR_MORE_DATA.to_hresult() => {
                stats.feed(&mut self.carry, data, false)
            }
            Err(e) if e.code() == ERROR_OPERATION_ABORTED.to_hresult() => return false,
            Err(_) => {
                self.pending = Pending::None;
                self.reconnect(stats);
                return false;
            }
        }
        true
    }

    /// O evento desta instância sinalizou.
    fn on_signal(&mut self, stats: &mut Stats) {
        match self.pending {
            Pending::Connect => {
                let mut n = 0u32;
                // SAFETY: operação de conexão concluída sobre `ov`.
                let r = unsafe {
                    GetOverlappedResult(h(&self.pipe), &raw const *self.ov, &raw mut n, false)
                };
                self.pending = Pending::None;
                if r.is_ok() {
                    self.on_connected(stats)
                } else {
                    self.reconnect(stats)
                }
            }
            Pending::Read => {
                self.pending = Pending::None;
                if self.complete_read(stats) {
                    self.start_read(stats);
                }
            }
            Pending::None => {}
        }
    }

    /// Cancela I/O pendente e processa o que já tinha chegado antes do cancelamento.
    fn shutdown(&mut self, stats: &mut Stats) {
        if self.pending == Pending::None {
            return;
        }
        let mut n = 0u32;
        // SAFETY: cancela a operação sobre `ov` e espera a conclusão antes de soltar o Box.
        let r = unsafe {
            let _ = CancelIoEx(h(&self.pipe), Some(&raw const *self.ov));
            GetOverlappedResult(h(&self.pipe), &raw const *self.ov, &raw mut n, true)
        };
        if self.pending == Pending::Read
            && (r.is_ok()
                || r.as_ref()
                    .is_err_and(|e| e.code() == ERROR_MORE_DATA.to_hresult()))
        {
            stats.feed(&mut self.carry, &self.buf[..n as usize], r.is_ok());
        }
        self.pending = Pending::None;
    }
}

fn security_attributes() -> Result<(SECURITY_ATTRIBUTES, PSECURITY_DESCRIPTOR), String> {
    let sid = user_sid().ok_or("SID do usuário")?;
    // DACL protegida: SYSTEM e Administradores com controle total; o usuário (jogo sem
    // elevação) com leitura e escrita (GRGW), sem WRITE_DAC/WRITE_OWNER. GW inclui
    // FILE_APPEND_DATA (= FILE_CREATE_PIPE_INSTANCE), necessário para clientes que abrem com
    // GENERIC_WRITE; o limite fixo de instâncias (todas criadas por nós) impede instâncias
    // extras. Rótulo de integridade média explícito.
    let sddl = common::wide(&format!(
        "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;{sid})S:(ML;;NW;;;ME)"
    ));
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: SDDL terminado em zero; o descritor é liberado por quem chamou.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &raw mut sd,
            None,
        )
    }
    .map_err(|e| format!("SDDL: {e}"))?;
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: false.into(),
    };
    Ok((sa, sd))
}

fn run(secs: u64) {
    let (sa, sd) = match security_attributes() {
        Ok(x) => x,
        Err(e) => return println!("FALHA: {e}"),
    };
    let mut instances = Vec::new();
    for i in 0..INSTANCES {
        match Instance::create(i == 0, &sa) {
            Ok(inst) => instances.push(inst),
            Err(e) => {
                println!("FALHA: instância {i}: {e} (outro servidor do pipe já existe?)");
                break;
            }
        }
    }
    // SAFETY: as instâncias já foram criadas; o descritor não é mais usado.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(sd.0)));
    }
    if instances.is_empty() {
        return;
    }
    let mut stats = Stats::default();
    for inst in &mut instances {
        inst.start_connect(&mut stats);
    }
    println!(
        "Pipe com {} instâncias; aguardando o RTSS por {secs} s...",
        instances.len()
    );

    let shm = common::SharedView::open("RTSSSharedMemoryV2");
    let mut shm_samples: BTreeMap<(u32, String), Vec<u32>> = BTreeMap::new();
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut last_sample = Instant::now();
    let mut last_print = Instant::now();
    while Instant::now() < deadline {
        let events: Vec<HANDLE> = instances.iter().map(|i| h(&i.event)).collect();
        let left = deadline
            .saturating_duration_since(Instant::now())
            .as_millis();
        let timeout = left.min(250) as u32;
        // SAFETY: eventos válidos (no máximo 64).
        let r = unsafe { WaitForMultipleObjects(&events, false, timeout) };
        let idx = r.0.wrapping_sub(WAIT_OBJECT_0.0) as usize;
        if idx < instances.len() {
            instances[idx].on_signal(&mut stats);
        }
        // Fallback: amostra o frametime do jogo na Shared Memory a cada 250 ms.
        if last_sample.elapsed() >= Duration::from_millis(250) {
            last_sample = Instant::now();
            if let Some(Ok((apps, fg))) = shm.as_ref().map(read_apps)
                && let Some(g) = select_game(&apps, fg)
                && g.frametime_us > 0
            {
                shm_samples
                    .entry((g.pid, g.name.clone()))
                    .or_default()
                    .push(g.frametime_us);
            }
        }
        if last_print.elapsed() >= Duration::from_secs(1) {
            last_print = Instant::now();
            for (app, (n, sum, max)) in &stats.second {
                println!(
                    "  pipe dwApp {app:>6}: {n:>4} frames/s  méd {:.2} ms  máx {:.2} ms",
                    *sum as f64 / *n as f64 / 1000.0,
                    f64::from(*max) / 1000.0
                );
            }
            stats.second.clear();
        }
    }
    for inst in &mut instances {
        inst.shutdown(&mut stats);
    }

    println!("\nResumo do pipe (exato, por frame):");
    if stats.per_app.is_empty() {
        println!("  nenhum frame recebido");
    }
    for (app, s) in &stats.per_app {
        let total: u64 = s.iter().map(|&u| u64::from(u)).sum();
        let lows = lows(s).map_or_else(
            || "lows: poucas amostras".into(),
            |(a, b)| format!("1% low {a:.1} FPS, 0,1% low {b:.1} FPS"),
        );
        println!(
            "  dwApp {app}: {} frames, {:.1} FPS médio, máx {:.2} ms, {lows}",
            s.len(),
            1e6 * s.len() as f64 / total.max(1) as f64,
            f64::from(s.iter().copied().max().unwrap_or(0)) / 1000.0
        );
    }
    if stats.dropped_bytes > 0 {
        println!(
            "  {} bytes descartados (mensagens não múltiplas de 8)",
            stats.dropped_bytes
        );
    }
    println!("\nFallback pela Shared Memory (amostras a cada 250 ms, aproximado):");
    if shm_samples.is_empty() {
        println!("  nenhum jogo ativo");
    }
    for ((pid, name), s) in &shm_samples {
        let lows = lows(s).map_or_else(
            || "lows: < 100 amostras".into(),
            |(a, b)| format!("1% low ~{a:.1} FPS, 0,1% low ~{b:.1} FPS"),
        );
        println!("  PID {pid} {name}: {} amostras, {lows}", s.len());
    }
}

fn main() {
    common::harden_dll_search();
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    println!("Elevado: {}\n", common::is_elevated());
    snapshot("RTSS antes:");
    println!();
    run(secs);
    println!();
    snapshot("RTSS depois:");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(pid: u32, name: &str, api: u32, fps: f64, age: Option<u32>) -> App {
        App {
            pid,
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
        // `t1` atualizado depois de lido o relógio: idade 0, não 49 dias.
        assert_eq!(age_ms(1000, 1001), Some(0));
        // volta do GetTickCount (49,7 dias)
        assert_eq!(age_ms(5, u32::MAX - 4), Some(10));
    }

    #[test]
    fn escolha_do_jogo() {
        let apps = vec![
            app(1, "spotifast.exe", 1, 100.0, Some(10)),
            app(2, "deadlock.exe", 6, 33.0, Some(40)),
            app(3, "outro.exe", 7, 120.0, Some(9000)), // parado
            app(4, "semapi.exe", 0, 60.0, Some(10)),
        ];
        assert_eq!(select_game(&apps, Some(1)).map(|a| a.pid), Some(2)); // ignorado não vence
        assert_eq!(select_game(&apps, None).map(|a| a.pid), Some(2));
        assert_eq!(select_game(&apps[2..], None).map(|a| a.pid), None);
    }

    #[test]
    fn lows_basicos() {
        assert_eq!(lows(&[16_000; 50]), None);
        let mut s = vec![10_000u32; 990];
        s.extend([50_000u32; 10]); // 1% de frames a 50 ms
        let (l1, l01) = lows(&s).unwrap();
        assert!((l1 - 20.0).abs() < 1e-9);
        assert!((l01 - 20.0).abs() < 1e-9);
    }

    #[test]
    fn registros_partidos_entre_mensagens() {
        let mut st = Stats::default();
        let mut carry = Vec::new();
        let mut rec = 7u32.to_le_bytes().to_vec();
        rec.extend(16_000u32.to_le_bytes());
        st.feed(&mut carry, &rec[..5], false); // ERROR_MORE_DATA no meio do registro
        st.feed(&mut carry, &rec[5..], true);
        assert_eq!(st.per_app[&7], vec![16_000]);
        st.feed(&mut carry, &rec[..3], true); // mensagem malformada
        assert_eq!(st.dropped_bytes, 3);
        assert!(carry.is_empty());
    }
}
