//! Leitura da Shared Memory RTSSSharedMemoryV2, seleção do jogo em foco e partida do RTSS.

use std::os::windows::process::CommandExt;

use windows::Win32::System::SystemInformation::GetTickCount;

use crate::shm::SharedView;
use crate::win;
// RTSS_SHARED_MEMORY
pub(super) const SHM_NAME: &str = "RTSSSharedMemoryV2";
pub(super) const RTSS_SIGNATURE: u32 = 0x5254_5353; // 'RTSS'
pub(super) const RTSS_DEAD: u32 = 0xDEAD;
pub(super) const OFF_VERSION: usize = 4;
pub(super) const OFF_APP_ENTRY_SIZE: usize = 8;
pub(super) const OFF_APP_ARR_OFFSET: usize = 12;
pub(super) const OFF_APP_ARR_SIZE: usize = 16;
pub(super) const OFF_LAST_FOREGROUND_PID: usize = 68; // v2.16+
// RTSS_SHARED_MEMORY_APP_ENTRY (offsets do compilador MSVC, header v2.21)
pub(super) const E_PID: usize = 0;
pub(super) const E_NAME: usize = 4;
pub(super) const E_FLAGS: usize = 264;
pub(super) const E_TIME0: usize = 268;
pub(super) const E_TIME1: usize = 272;
pub(super) const E_FRAMES: usize = 276;
pub(super) const E_FRAME_TIME: usize = 280;
pub(super) const E_RES_X: usize = 9224; // v2.20+
pub(super) const E_RES_Y: usize = 9228;
pub(super) const E_GPU_ACTIVE: usize = 9336; // v2.21+
pub(super) const E_GPU_FRAME: usize = 9340;
pub(super) const V2_16: u32 = 0x0002_0010;
pub(super) const V2_20: u32 = 0x0002_0014;
pub(super) const V2_21: u32 = 0x0002_0015;
pub(super) const MAX_APPS: usize = 256;
/// Idade máxima do último período de FPS para o app contar como "ativo".
pub(super) const ACTIVE_MS: u32 = 2000;
const RTSS_KEY: &str = r"SOFTWARE\Unwinder\RTSS";

pub(super) fn api_name(flags: u32) -> &'static str {
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
pub(super) struct App {
    /// Índice da entrada na Shared Memory (= `dwApp` do pipe).
    pub(super) index: u32,
    pub(super) pid: u32,
    pub(super) name: String,
    pub(super) api: u32,
    pub(super) fps: f64,
    pub(super) frametime_us: u32,
    /// ms desde o fim do último período de FPS; `None` = nenhum período registrado.
    pub(super) age_ms: Option<u32>,
    pub(super) res: Option<(u32, u32)>,
    pub(super) gpu: Option<(u32, u32)>,
}

/// Idade sem underflow: `now` é lido depois de `t1`; diferença "negativa" = 0.
pub(super) fn age_ms(now: u32, t1: u32) -> Option<u32> {
    if t1 == 0 {
        return None;
    }
    let d = now.wrapping_sub(t1);
    Some(if d > i32::MAX as u32 { 0 } else { d })
}

/// Aplicativos da Shared Memory, PID em primeiro plano e versão.
pub(super) fn read_apps(v: &SharedView) -> Result<(Vec<App>, Option<u32>, u32), String> {
    // SAFETY: sem pré-condições; lido a cada entrada, depois de `t1` (ver `age_ms`).
    read_apps_at(v, || unsafe { GetTickCount() })
}

/// Como `read_apps`, com o relógio dado (o fuzz usa um instante fixo: sementes reproduzíveis).
pub(super) fn read_apps_at(
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
pub(super) fn is_ignored(list: &[String], name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    list.contains(&n)
}

/// Jogo atual: ativo, com API 3D e fora da lista de ignorados; o primeiro plano desempata,
/// senão o de maior FPS.
pub(super) fn select_game<'a>(
    apps: &'a [App],
    fg: Option<u32>,
    ignored: &[String],
) -> Option<&'a App> {
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

/// PIDs com o hook do RTSS agora (leitura direta da Shared Memory, sem esperar a thread do
/// RTSS): a última checagem antes de abrir um processo sob demanda.
pub fn hooked_now() -> Result<Vec<u32>, String> {
    let v = SharedView::open(SHM_NAME).ok_or("RTSS shared memory unavailable")?;
    let (apps, _, _) = read_apps(&v)?;
    Ok(apps.iter().map(|a| a.pid).collect())
}

/// Inicia o RTSS pelo caminho do registro (visão de 32 bits; o RTSS é um programa de 32 bits).
pub(super) fn start_rtss() -> Result<(), String> {
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
