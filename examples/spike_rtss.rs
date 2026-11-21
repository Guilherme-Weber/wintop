//! Spike da Etapa 0: RTSS (Shared Memory v2.21 e pipe de frametime).
//!
//! 1. Lista os aplicativos que o RTSS acompanha (`RTSSSharedMemoryV2`), com FPS, frametime,
//!    API, resolução e tempos de GPU (offsets gerados pelo compilador a partir do header
//!    oficial: `sizeof(RTSS_SHARED_MEMORY_APP_ENTRY)` = 12416, igual ao `dwAppEntrySize`).
//! 2. Cria o servidor `\\.\pipe\RTSS_Frametime` com DACL restrita (SYSTEM, Administradores e o
//!    usuário atual), rótulo de integridade média explícito, I/O overlapped e sem clientes
//!    remotos; recebe `{dwApp, dwFrametime}` por frame e agrega por segundo.
//!
//! `cargo run --example spike_rtss [segundos]`

#[path = "common/mod.rs"]
mod common;

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_MORE_DATA, ERROR_PIPE_CONNECTED,
    HANDLE, HLOCAL, LocalFree, WAIT_OBJECT_0,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows::Win32::Storage::FileSystem::{
    FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX, ReadFile,
};
use windows::Win32::System::IO::{CancelIo, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId,
    PIPE_READMODE_MESSAGE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_MESSAGE, PIPE_UNLIMITED_INSTANCES,
    PIPE_WAIT,
};
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW, WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR, w};

// RTSS_SHARED_MEMORY
const RTSS_SIGNATURE: u32 = 0x5254_5353; // 'RTSS'
const OFF_VERSION: usize = 4;
const OFF_APP_ENTRY_SIZE: usize = 8;
const OFF_APP_ARR_OFFSET: usize = 12;
const OFF_APP_ARR_SIZE: usize = 16;
const OFF_LAST_FOREGROUND_PID: usize = 68;
// RTSS_SHARED_MEMORY_APP_ENTRY (offsets do compilador MSVC, header v2.21)
const E_PID: usize = 0;
const E_NAME: usize = 4;
const E_FLAGS: usize = 264;
const E_TIME0: usize = 268;
const E_TIME1: usize = 272;
const E_FRAMES: usize = 276;
const E_FRAME_TIME: usize = 280;
const E_STAT_FLAGS: usize = 284;
const E_LOW_1: usize = 9176;
const E_LOW_01: usize = 9180;
const E_RES_X: usize = 9224;
const E_RES_Y: usize = 9228;
const E_GPU_ACTIVE: usize = 9336;
const E_GPU_FRAME: usize = 9340;
const ENTRY_SIZE_V2_21: u32 = 12416;

fn api_name(flags: u32) -> String {
    match flags & 0xFFFF {
        0 => "-".into(),
        0x1 => "OpenGL".into(),
        0x2 => "DirectDraw".into(),
        0x3 => "D3D8".into(),
        0x4 => "D3D9".into(),
        0x5 => "D3D9Ex".into(),
        0x6 => "D3D10".into(),
        0x7 => "D3D11".into(),
        0x8 => "D3D12".into(),
        0x9 => "D3D12 AFR".into(),
        0xA => "Vulkan".into(),
        n => format!("API {n:#x}"),
    }
}

fn snapshot() {
    let Some(v) = common::SharedView::open("RTSSSharedMemoryV2") else {
        return println!("RTSS: Shared Memory ausente (RTSS fechado?)");
    };
    let read = || -> Option<()> {
        let sig = v.u32_at(0)?;
        let ver = v.u32_at(OFF_VERSION)?;
        let esize = v.u32_at(OFF_APP_ENTRY_SIZE)?;
        let arr = v.u32_at(OFF_APP_ARR_OFFSET)? as usize;
        let count = v.u32_at(OFF_APP_ARR_SIZE)? as usize;
        println!(
            "RTSS: assinatura {} | versão {}.{} | entrada {esize} B (layout v2.21 = {ENTRY_SIZE_V2_21}) | view {} B",
            if sig == RTSS_SIGNATURE {
                "OK"
            } else {
                "INVÁLIDA"
            },
            ver >> 16,
            ver & 0xFFFF,
            v.len
        );
        if sig != RTSS_SIGNATURE || esize < ENTRY_SIZE_V2_21 {
            return None;
        }
        let fg = v.u32_at(OFF_LAST_FOREGROUND_PID)?;
        // SAFETY: sem pré-condições.
        let now = unsafe { GetTickCount() };
        println!("Último app em primeiro plano: PID {fg}");
        for i in 0..count {
            let e = arr + i * esize as usize;
            let pid = v.u32_at(e + E_PID)?;
            if pid == 0 {
                continue;
            }
            let name = v.cstr(e + E_NAME, 260)?;
            let name = name.rsplit('\\').next().unwrap_or(&name).to_string();
            let (t0, t1, frames) = (
                v.u32_at(e + E_TIME0)?,
                v.u32_at(e + E_TIME1)?,
                v.u32_at(e + E_FRAMES)?,
            );
            let span = t1.wrapping_sub(t0);
            let fps = if t0 != 0 && span > 0 {
                1000.0 * f64::from(frames) / f64::from(span)
            } else {
                0.0
            };
            println!(
                "  PID {pid:>6} {name:<28} {:<9} {fps:>6.1} FPS  frametime {:>7.2} ms  idade {:>6} ms  \
                 res {}x{}  low1% {} low0.1% {} stat {:#x}  GPU ativo {} µs / frame GPU {} µs",
                api_name(v.u32_at(e + E_FLAGS)?),
                f64::from(v.u32_at(e + E_FRAME_TIME)?) / 1000.0,
                now.wrapping_sub(t1),
                v.u32_at(e + E_RES_X)?,
                v.u32_at(e + E_RES_Y)?,
                v.u32_at(e + E_LOW_1)?,
                v.u32_at(e + E_LOW_01)?,
                v.u32_at(e + E_STAT_FLAGS)?,
                v.u32_at(e + E_GPU_ACTIVE)?,
                v.u32_at(e + E_GPU_FRAME)?,
            );
        }
        Some(())
    };
    if read().is_none() {
        println!("RTSS: leitura fora dos limites ou layout inesperado");
    }
}

/// SID do usuário do token atual, em texto (S-1-5-21-...).
fn user_sid() -> Option<String> {
    let mut token = HANDLE::default();
    // SAFETY: buffers locais; o handle e o texto do SID são liberados aqui.
    unsafe {
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token).ok()?;
        let mut buf = vec![0u8; 256];
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr().cast::<c_void>()),
            buf.len() as u32,
            &raw mut len,
        );
        let _ = CloseHandle(token);
        ok.ok()?;
        let user = buf.as_ptr().cast::<TOKEN_USER>().read_unaligned();
        let mut text = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &raw mut text).ok()?;
        let s = text.to_string().ok();
        let _ = LocalFree(Some(HLOCAL(text.0.cast())));
        s
    }
}

fn process_name(pid: u32) -> String {
    // SAFETY: handle fechado logo após o uso; buffer local.
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return "?".into();
        };
        let mut buf = [0u16; 260];
        let mut len = buf.len() as u32;
        let r = QueryFullProcessImageNameW(
            h,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &raw mut len,
        );
        let _ = CloseHandle(h);
        if r.is_err() {
            return "?".into();
        }
        let full = String::from_utf16_lossy(&buf[..len as usize]);
        full.rsplit('\\').next().unwrap_or(&full).to_string()
    }
}

#[derive(Default)]
struct Agg {
    frames: u64,
    sum_us: u64,
    min_us: u32,
    max_us: u32,
}

impl Agg {
    fn add(&mut self, us: u32) {
        if self.frames == 0 || us < self.min_us {
            self.min_us = us;
        }
        self.max_us = self.max_us.max(us);
        self.frames += 1;
        self.sum_us += u64::from(us);
    }
}

fn run_pipe(secs: u64) {
    let Some(sid) = user_sid() else {
        return println!("FALHA: SID do usuário");
    };
    // DACL protegida: SYSTEM, Administradores e o usuário atual; rótulo de integridade média.
    let sddl = format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{sid})S:(ML;;NW;;;ME)");
    let sddl_w = common::wide(&sddl);
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: SDDL terminado em zero; o descritor é liberado no fim.
    if let Err(e) = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl_w.as_ptr()),
            SDDL_REVISION_1,
            &raw mut sd,
            None,
        )
    } {
        return println!("FALHA: SDDL: {e}");
    }
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: false.into(),
    };
    // SAFETY: atributos válidos durante a chamada.
    let pipe = unsafe {
        CreateNamedPipeW(
            w!(r"\\.\pipe\RTSS_Frametime"),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            65536,
            65536,
            0,
            Some(&raw const sa),
        )
    };
    if pipe.is_invalid() {
        // SAFETY: descritor alocado acima.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(sd.0)));
        }
        return println!(
            "FALHA: CreateNamedPipe: {} (outro servidor já existe?)",
            windows::core::Error::from_thread()
        );
    }
    // SAFETY: evento manual, fechado no fim.
    let Ok(event) = (unsafe { CreateEventW(None, true, false, None) }) else {
        return println!("FALHA: CreateEvent");
    };
    println!("Pipe criado; aguardando o RTSS por {secs} s...");

    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut total: BTreeMap<u32, Agg> = BTreeMap::new();
    let mut all_us: Vec<u32> = Vec::new();
    let mut buf = vec![0u8; 65536];
    let remaining_ms = |d: Instant| d.saturating_duration_since(Instant::now()).as_millis() as u32;

    'connect: while Instant::now() < deadline {
        let mut ov = OVERLAPPED {
            hEvent: event,
            ..Default::default()
        };
        // SAFETY: `ov` vive até a conclusão (esperada abaixo ou cancelada com CancelIo).
        let connected = match unsafe { ConnectNamedPipe(pipe, Some(&raw mut ov)) } {
            Ok(()) => true,
            Err(e) if e.code() == ERROR_PIPE_CONNECTED.to_hresult() => true,
            Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {
                // SAFETY: evento válido.
                if unsafe { WaitForSingleObject(event, remaining_ms(deadline)) } == WAIT_OBJECT_0 {
                    let mut n = 0u32;
                    // SAFETY: operação concluída sobre `ov`.
                    unsafe { GetOverlappedResult(pipe, &raw const ov, &raw mut n, false) }.is_ok()
                } else {
                    let mut ignored = 0u32;
                    // SAFETY: cancela a operação pendente antes de `ov` sair de escopo.
                    unsafe {
                        let _ = CancelIo(pipe);
                        let _ = GetOverlappedResult(pipe, &raw const ov, &raw mut ignored, true);
                    }
                    false
                }
            }
            Err(e) => {
                println!("FALHA: ConnectNamedPipe: {e}");
                false
            }
        };
        if !connected {
            break;
        }
        let mut client = 0u32;
        // SAFETY: pipe conectado.
        let _ = unsafe { GetNamedPipeClientProcessId(pipe, &raw mut client) };
        println!("Cliente conectado: PID {client} ({})", process_name(client));

        let mut second: BTreeMap<u32, Agg> = BTreeMap::new();
        let mut tick = Instant::now();
        while Instant::now() < deadline {
            let mut ov = OVERLAPPED {
                hEvent: event,
                ..Default::default()
            };
            let mut n = 0u32;
            // SAFETY: `buf` e `ov` vivem até a conclusão ou o cancelamento.
            let r = unsafe { ReadFile(pipe, Some(&mut buf), None, Some(&raw mut ov)) };
            let done = match r {
                Ok(()) => true,
                Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {
                    // SAFETY: evento válido.
                    if unsafe { WaitForSingleObject(event, remaining_ms(deadline).min(1000)) }
                        == WAIT_OBJECT_0
                    {
                        true
                    } else {
                        // SAFETY: cancela antes de reutilizar `buf`/`ov`.
                        unsafe {
                            let _ = CancelIo(pipe);
                        }
                        false
                    }
                }
                Err(e) if e.code() == ERROR_BROKEN_PIPE.to_hresult() => {
                    println!("Cliente desconectou; reconectando");
                    // SAFETY: pipe válido.
                    unsafe {
                        let _ = DisconnectNamedPipe(pipe);
                    }
                    continue 'connect;
                }
                Err(e) => {
                    println!("FALHA: ReadFile: {e}");
                    break 'connect;
                }
            };
            // SAFETY: obtém o resultado (ou o cancelamento) da operação sobre `ov`.
            let res = unsafe { GetOverlappedResult(pipe, &raw const ov, &raw mut n, true) };
            if done {
                match res {
                    Ok(()) => {}
                    Err(e) if e.code() == ERROR_MORE_DATA.to_hresult() => {
                        println!("aviso: mensagem maior que o buffer")
                    }
                    Err(e) if e.code() == ERROR_BROKEN_PIPE.to_hresult() => {
                        println!("Cliente desconectou; reconectando");
                        // SAFETY: pipe válido.
                        unsafe {
                            let _ = DisconnectNamedPipe(pipe);
                        }
                        continue 'connect;
                    }
                    Err(_) => n = 0,
                }
                let (records, _) = buf[..n as usize].as_chunks::<8>();
                for rec in records {
                    let app = u32::from_le_bytes([rec[0], rec[1], rec[2], rec[3]]);
                    let us = u32::from_le_bytes([rec[4], rec[5], rec[6], rec[7]]);
                    second.entry(app).or_default().add(us);
                    total.entry(app).or_default().add(us);
                    all_us.push(us);
                }
            }
            if tick.elapsed() >= Duration::from_secs(1) {
                for (app, a) in &second {
                    println!(
                        "  dwApp {app:>6}: {:>4} frames/s  frametime méd {:.2} ms  mín {:.2}  máx {:.2}",
                        a.frames,
                        a.sum_us as f64 / a.frames as f64 / 1000.0,
                        f64::from(a.min_us) / 1000.0,
                        f64::from(a.max_us) / 1000.0
                    );
                }
                if second.is_empty() {
                    println!("  (nenhum frame neste segundo)");
                }
                second.clear();
                tick = Instant::now();
            }
        }
        break;
    }

    println!("\nResumo do pipe:");
    if total.is_empty() {
        println!("  nenhum frame recebido");
    }
    for (app, a) in &total {
        println!(
            "  dwApp {app}: {} frames, média {:.2} ms ({:.1} FPS), mín {:.2} ms, máx {:.2} ms",
            a.frames,
            a.sum_us as f64 / a.frames as f64 / 1000.0,
            1e6 * a.frames as f64 / a.sum_us.max(1) as f64,
            f64::from(a.min_us) / 1000.0,
            f64::from(a.max_us) / 1000.0
        );
    }
    if all_us.len() >= 100 {
        // 1% low: FPS médio do 1% de frames mais lentos (cálculo próprio, sem modo benchmark).
        all_us.sort_unstable_by(|a, b| b.cmp(a));
        let k = (all_us.len() / 100).max(1);
        let avg_worst = all_us[..k].iter().map(|&u| u64::from(u)).sum::<u64>() as f64 / k as f64;
        println!(
            "  1% low (todos os apps juntos): {:.1} FPS",
            1e6 / avg_worst
        );
    }
    // SAFETY: handles e descritor criados acima.
    unsafe {
        let _ = CloseHandle(event);
        let _ = CloseHandle(pipe);
        let _ = LocalFree(Some(HLOCAL(sd.0)));
    }
}

fn main() {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    println!("Elevado: {}\n", common::is_elevated());
    snapshot();
    println!();
    run_pipe(secs);
    println!();
    snapshot();
}
