//! Validação ponta a ponta com o `fps_probe` (exemplo que gera FPS, frametime e VRAM
//! conhecidos): o RTSS precisa estar aberto; o probe é iniciado depois do coletor, então o
//! frametime vem pelo pipe (valores exatos, não o fallback aproximado).
//!
//! Preparar: `cargo build --release --example fps_probe` e rodar com
//! `cargo test --release probe_ -- --ignored --test-threads=1 --nocapture` (terminal elevado).

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::CreateEventW;

use super::{RtssCollector, SlowCollectors};
use crate::model::View;

/// Cadência do teste: 100 FPS com engasgo de 50 ms a cada 50 frames (2% dos frames, então o
/// 1% e o 0,1% low são só os engasgos).
const FPS: f64 = 100.0;
const EVERY: u32 = 50;
const STUTTER_MS: f64 = 50.0;
const VRAM_MB: u64 = 256;

fn expected_avg_ms() -> f64 {
    let n = f64::from(EVERY);
    ((n - 1.0) * (1000.0 / FPS) + STUTTER_MS) / n
}

/// O executável do probe em `target\<perfil>\examples` (release antes de debug), subindo a
/// partir do executável de teste (que no layout novo do Cargo fica em `build\wintop\<hash>\out`).
fn probe_exe() -> PathBuf {
    let me = std::env::current_exe().expect("executável de teste");
    me.ancestors()
        .flat_map(|a| {
            [
                a.join("release").join("examples").join("fps_probe.exe"),
                a.join("debug").join("examples").join("fps_probe.exe"),
            ]
        })
        .find(|p| p.is_file())
        .expect("fps_probe.exe não encontrado: cargo build --release --example fps_probe")
}

struct Probe(Child);

impl Drop for Probe {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_probe(seconds: u32) -> Probe {
    let child = Command::new(probe_exe())
        .args([
            "--fps",
            &FPS.to_string(),
            "--stutter-every",
            &EVERY.to_string(),
            "--stutter-ms",
            &STUTTER_MS.to_string(),
            "--vram-mb",
            &VRAM_MB.to_string(),
            "--seconds",
            &seconds.to_string(),
        ])
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("iniciar o fps_probe");
    Probe(child)
}

#[test]
#[ignore = "hardware: RTSS aberto + fps_probe compilado (abre uma janela D3D11 por ~20 s)"]
fn probe_fps_frametime_e_lows() {
    // SAFETY: evento sem nome; o handle passa a ter dono na hora.
    let stop = unsafe {
        OwnedHandle::from_raw_handle(CreateEventW(None, true, false, None).expect("evento").0)
    };
    let stop = HANDLE(stop.as_raw_handle());
    let mut c = RtssCollector::new(false, true);
    let first = c.sample();
    assert!(first.rtss.is_ok(), "RTSS fechado: {:?}", first.rtss);
    let probe = start_probe(25);
    let pid = probe.0.id();
    let t0 = Instant::now();
    let (mut lo, mut hi) = (f64::MAX, 0.0f64);
    let mut fps = Vec::new();
    let mut last = None;
    let mut chosen = None;
    while t0.elapsed() < Duration::from_secs(18) {
        let s = c.sample();
        if let Ok(g) = &s.game {
            chosen = Some((g.pid, g.name.clone()));
            if g.pid == pid {
                // Ignora os primeiros 3 s (janela abrindo, hook entrando).
                if t0.elapsed() > Duration::from_secs(3) {
                    if let Ok(f) = g.fps {
                        fps.push(f);
                    }
                    if let Ok((_, a, b)) = g.frametime {
                        lo = lo.min(a);
                        hi = hi.max(b);
                    }
                }
                last = Some(g.clone());
            }
        }
        c.pump(Instant::now() + Duration::from_millis(250), stop);
    }
    let g = last.unwrap_or_else(|| panic!("probe (PID {pid}) não escolhido; jogo: {chosen:?}"));
    let avg = fps.iter().sum::<f64>() / fps.len().max(1) as f64;
    let want = 1000.0 / expected_avg_ms();
    let want_low = 1000.0 / STUTTER_MS;
    println!(
        "API {}; FPS médio {avg:.2} (esperado {want:.2}); frametime {lo:.2}-{hi:.2} ms \
         (esperado {:.2}-{STUTTER_MS:.2}); lows {:?} (esperado {want_low:.2}); aprox. {}",
        g.api,
        1000.0 / FPS,
        g.lows,
        g.approx
    );
    assert!(
        !g.approx,
        "frametime pelo pipe (probe aberto depois do coletor)"
    );
    assert!(g.api.starts_with("D3D1"), "{}", g.api);
    assert!((avg - want).abs() < want * 0.02, "FPS {avg} × {want}");
    // Jitter do próprio probe: um Present que bloqueia (fila da GPU cheia) atrasa um frame e o
    // seguinte "recupera" (medido: 8.1 ms num alvo de 10 ms). O RTSS mede o que aconteceu.
    assert!((lo - 1000.0 / FPS).abs() < 2.5, "frametime mínimo {lo}");
    assert!((hi - STUTTER_MS).abs() < 2.5, "frametime máximo {hi}");
    let (l1, l01) = g.lows.expect("lows com mais de 100 frames");
    assert!((l1 - want_low).abs() < want_low * 0.03, "1% low {l1}");
    assert!((l01 - want_low).abs() < want_low * 0.03, "0.1% low {l01}");
}

#[test]
#[ignore = "hardware: fps_probe compilado; GPU com WDDM (abre uma janela D3D11 por ~10 s)"]
fn probe_aparece_na_aba_gpu() {
    let probe = start_probe(10);
    let pid = probe.0.id();
    std::thread::sleep(Duration::from_secs(3));
    let mut slow = SlowCollectors::new(super::shared_nvml());
    let view = View {
        procs: true,
        gpu: true,
    };
    let s = slow.collect(&std::sync::atomic::AtomicBool::new(false), view);
    let gpus = s.gpus.expect("GPUs");
    let procs = gpus[0].procs.clone().expect("aba GPU").expect("processos");
    let p = procs
        .iter()
        .find(|p| p.pid == pid)
        .unwrap_or_else(|| panic!("probe ausente: {procs:?}"));
    let engines = p.engines.clone().expect("engines");
    let three_d = engines
        .iter()
        .find(|(k, _)| k == "3D")
        .map_or(0.0, |(_, v)| *v);
    println!(
        "{}: local {} MB, não local {} MB, confirmada {} MB, engines {engines:?}",
        p.name,
        p.local >> 20,
        p.non_local >> 20,
        p.committed >> 20
    );
    assert!(p.local >= VRAM_MB << 20, "VRAM local {}", p.local);
    assert!(three_d > 0.0, "engine 3D {engines:?}");
    let list = s.procs.expect("aba Processos").expect("processos");
    let row = list.iter().find(|r| r.pid == pid).expect("probe na lista");
    // O laço de espera ocupa a CPU só no último ~1 ms de cada frame.
    assert!(row.cpu < 100.0 && row.threads >= 1, "{row:?}");
}
