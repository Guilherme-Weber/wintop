//! Gerador de FPS conhecido para validar o wintop (RTSS e aba GPU).
//!
//! Abre uma janela D3D11 (o RTSS injeta o hook nela como num jogo) e apresenta frames numa
//! cadência exata: `--fps` frames por segundo, com um engasgo de `--stutter-ms` a cada
//! `--stutter-every` frames. Aloca `--vram-mb` de texturas na VRAM (aparece como VRAM local do
//! processo). Os valores esperados saem no console e em `expected()`, usado pelos testes:
//!
//! - frametime médio = ((N-1)·T + S) / N, FPS = 1000 / médio;
//! - 1% e 0,1% low = 1000 / S quando os engasgos são >= 1% dos frames (N <= 100).
//!
//! Uso: `cargo run --release --example fps_probe -- [--fps 100] [--stutter-every 100]
//! [--stutter-ms 50] [--vram-mb 256] [--seconds 30] [--freeze-at 8 --freeze-ms 1500]`

use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11CreateDeviceAndSwapChain, ID3D11Device, ID3D11DeviceContext,
    ID3D11RenderTargetView, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_MODE_DESC, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_PRESENT, DXGI_SWAP_CHAIN_DESC, DXGI_SWAP_EFFECT_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
    IDXGISwapChain,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, INFINITE, SetWaitableTimer,
    TIMER_ALL_ACCESS, WaitForSingleObject,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DispatchMessageW, MSG,
    PM_REMOVE, PeekMessageW, PostQuitMessage, RegisterClassW, SW_SHOW, SetForegroundWindow,
    ShowWindow, TranslateMessage, WM_DESTROY, WM_QUIT, WNDCLASSW, WS_EX_LEFT, WS_OVERLAPPEDWINDOW,
};
use windows::core::w;

struct Args {
    fps: f64,
    stutter_every: u32,
    stutter_ms: f64,
    vram_mb: u32,
    seconds: f64,
    /// Um congelamento único de `freeze_ms` aos `freeze_at` segundos (0 = nenhum).
    freeze_at: f64,
    freeze_ms: f64,
}

fn num<T: std::str::FromStr>(key: &str, val: &str) -> T {
    val.parse()
        .unwrap_or_else(|_| panic!("invalid value for {key}: {val}"))
}

fn args() -> Args {
    let mut a = Args {
        fps: 100.0,
        stutter_every: 100,
        stutter_ms: 50.0,
        vram_mb: 256,
        seconds: 30.0,
        freeze_at: 0.0,
        freeze_ms: 0.0,
    };
    let v: Vec<String> = std::env::args().skip(1).collect();
    for p in v.chunks(2) {
        let val = p.get(1).map_or("", String::as_str);
        match p[0].as_str() {
            "--fps" => a.fps = num(&p[0], val),
            "--stutter-every" => a.stutter_every = num(&p[0], val),
            "--stutter-ms" => a.stutter_ms = num(&p[0], val),
            "--vram-mb" => a.vram_mb = num(&p[0], val),
            "--seconds" => a.seconds = num(&p[0], val),
            "--freeze-at" => a.freeze_at = num(&p[0], val),
            "--freeze-ms" => a.freeze_ms = num(&p[0], val),
            o => panic!("unknown argument {o}"),
        }
    }
    a
}

extern "system" fn wndproc(h: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if msg == WM_DESTROY {
        // SAFETY: sem pré-condições.
        unsafe { PostQuitMessage(0) };
        return LRESULT(0);
    }
    // SAFETY: repassa a mensagem recebida.
    unsafe { DefWindowProcW(h, msg, w, l) }
}

/// Espera até `deadline`: timer de alta resolução até ~1 ms antes, depois laço ocupado.
fn wait_until(timer: windows::Win32::Foundation::HANDLE, deadline: Instant) {
    let now = Instant::now();
    if deadline > now + Duration::from_millis(1) {
        let d = deadline - now - Duration::from_millis(1);
        // Negativo = relativo, em 100 ns.
        let due = -((d.as_nanos() / 100) as i64);
        // SAFETY: timer válido; `due` local.
        unsafe {
            if SetWaitableTimer(timer, &raw const due, 0, None, None, false).is_ok() {
                WaitForSingleObject(timer, INFINITE);
            }
        }
    }
    while Instant::now() < deadline {
        std::hint::spin_loop();
    }
}

fn main() -> windows::core::Result<()> {
    let a = args();
    let t = 1000.0 / a.fps;
    let n = f64::from(a.stutter_every.max(1));
    let avg = ((n - 1.0) * t + a.stutter_ms) / n;
    println!(
        "fps_probe pid {}: {:.2} FPS esperado (frametime médio {avg:.3} ms; {t:.3} ms com \
         engasgo de {} ms a cada {} frames); 1% low {:.2}; VRAM {} MB; {} s",
        std::process::id(),
        1000.0 / avg,
        a.stutter_ms,
        a.stutter_every,
        1000.0 / a.stutter_ms,
        a.vram_mb,
        a.seconds
    );
    // SAFETY: chamadas Win32 com estruturas locais; os objetos COM são liberados no Drop.
    unsafe {
        let inst = GetModuleHandleW(None)?;
        let class = w!("wintop_fps_probe");
        RegisterClassW(&WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: inst.into(),
            lpszClassName: class,
            ..Default::default()
        });
        let hwnd = CreateWindowExW(
            WS_EX_LEFT,
            class,
            w!("wintop fps_probe"),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            640,
            360,
            None,
            None,
            Some(inst.into()),
            None,
        )?;
        let _ = ShowWindow(hwnd, SW_SHOW);
        // Em primeiro plano: o wintop escolhe o jogo pelo foco quando há mais de um app 3D.
        let _ = SetForegroundWindow(hwnd);

        let desc = DXGI_SWAP_CHAIN_DESC {
            BufferDesc: DXGI_MODE_DESC {
                Width: 640,
                Height: 360,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                ..Default::default()
            },
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            OutputWindow: hwnd,
            Windowed: true.into(),
            SwapEffect: DXGI_SWAP_EFFECT_DISCARD,
            ..Default::default()
        };
        let mut swap: Option<IDXGISwapChain> = None;
        let mut dev: Option<ID3D11Device> = None;
        let mut ctx: Option<ID3D11DeviceContext> = None;
        D3D11CreateDeviceAndSwapChain(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            Default::default(),
            D3D11_CREATE_DEVICE_FLAG(0),
            None,
            D3D11_SDK_VERSION,
            Some(&raw const desc),
            Some(&raw mut swap),
            Some(&raw mut dev),
            None,
            Some(&raw mut ctx),
        )?;
        let (swap, dev, ctx) = (swap.unwrap(), dev.unwrap(), ctx.unwrap());
        let back: ID3D11Texture2D = swap.GetBuffer(0)?;
        let mut rtv: Option<ID3D11RenderTargetView> = None;
        dev.CreateRenderTargetView(&back, None, Some(&raw mut rtv))?;
        let rtv = rtv.unwrap();

        // VRAM: texturas de 64 MB (4096 × 4096 RGBA8), limpas uma vez (ficam residentes).
        let mut keep = Vec::new();
        for _ in 0..a.vram_mb.div_ceil(64) {
            let td = D3D11_TEXTURE2D_DESC {
                Width: 4096,
                Height: 4096,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                ..Default::default()
            };
            let mut tex: Option<ID3D11Texture2D> = None;
            dev.CreateTexture2D(&raw const td, None, Some(&raw mut tex))?;
            let tex = tex.unwrap();
            let mut v: Option<ID3D11RenderTargetView> = None;
            dev.CreateRenderTargetView(&tex, None, Some(&raw mut v))?;
            let v = v.unwrap();
            ctx.ClearRenderTargetView(&v, &[0.1, 0.2, 0.3, 1.0]);
            keep.push((tex, v));
        }

        let timer = CreateWaitableTimerExW(
            None,
            None,
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
            TIMER_ALL_ACCESS.0,
        )?;
        let start = Instant::now();
        let end = start + Duration::from_secs_f64(a.seconds);
        let mut next = start;
        let mut frame: u64 = 0;
        let mut frozen = false;
        let mut msg = MSG::default();
        while Instant::now() < end {
            while PeekMessageW(&raw mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    return Ok(());
                }
                let _ = TranslateMessage(&raw const msg);
                DispatchMessageW(&raw const msg);
            }
            frame += 1;
            let freeze =
                a.freeze_ms > 0.0 && !frozen && (next - start).as_secs_f64() >= a.freeze_at;
            frozen |= freeze;
            let ms = if freeze {
                a.freeze_ms
            } else if frame.is_multiple_of(u64::from(a.stutter_every.max(1))) {
                a.stutter_ms
            } else {
                t
            };
            next += Duration::from_secs_f64(ms / 1000.0);
            let c = (frame % 120) as f32 / 120.0;
            ctx.ClearRenderTargetView(&rtv, &[c, 0.3, 1.0 - c, 1.0]);
            wait_until(timer, next);
            let _ = swap.Present(0, DXGI_PRESENT(0));
        }
        println!("{frame} frames em {:.2} s", start.elapsed().as_secs_f64());
    }
    Ok(())
}
