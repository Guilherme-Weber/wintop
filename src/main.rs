//! wintop: monitor de sistema em terminal para Windows.
//!
//! Etapa 1: coletores nativos (CPU, memória, discos, rede, GPU), motor com duas threads e
//! `wintop --check`. A interface (TUI) entra na Etapa 5; até lá, sem argumentos, o wintop
//! imprime um resumo a cada coleta. Ver `docs/metricas.md`.

mod check;
mod collect;
mod engine;
mod fmt;
mod instance;
mod model;
mod pdh;
mod shm;
mod win;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use windows::Win32::System::Console::{
    CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, SetConsoleCtrlHandler,
};
use windows::core::BOOL;

/// Pedido de parada (Ctrl+C, Ctrl+Break ou fechar a janela do console).
static STOP: AtomicBool = AtomicBool::new(false);

/// Roda numa thread criada pelo Windows: só marca a parada; o laço principal encerra o motor
/// (as threads de coleta terminam e os handles são fechados) e sai de `main`.
unsafe extern "system" fn on_console_event(kind: u32) -> BOOL {
    if matches!(kind, CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT) {
        STOP.store(true, Ordering::SeqCst);
        // Ao fechar a janela o Windows encerra o processo quando o handler retorna: espera o
        // laço principal terminar (ele vê o pedido em até 250 ms).
        if kind == CTRL_CLOSE_EVENT {
            std::thread::sleep(Duration::from_millis(1500));
        }
        BOOL(1)
    } else {
        BOOL(0)
    }
}

const HELP: &str = "\
usage: wintop [option]

  (no option)   print a summary on every collection (the full UI arrives in stage 5)
  --check       collect twice and show the status of every metric
  --version     print the version
  --help        show this help";

fn main() {
    win::harden_dll_search();
    let arg = std::env::args().nth(1);
    match arg.as_deref() {
        Some("--version" | "-V") => {
            println!("wintop {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Some("--help" | "-h") => {
            println!("{HELP}");
            return;
        }
        Some("--check") | None => {}
        Some(other) => {
            eprintln!("unknown option: {other}\n\n{HELP}");
            std::process::exit(2);
        }
    }

    let _instance = match instance::acquire() {
        Ok(guard) => guard,
        Err(instance::Error::AlreadyRunning) => {
            eprintln!("wintop is already running in this session.");
            std::process::exit(1);
        }
        Err(instance::Error::Os(e)) => {
            eprintln!("Failed to check for a running instance: {e}");
            std::process::exit(1);
        }
    };

    if arg.is_some() {
        std::process::exit(check::run());
    }
    summary_loop();
}

/// Resumo de uma linha a cada coleta da thread rápida (provisório, até a TUI).
fn summary_loop() {
    // Ctrl+C pode chegar desligado, herdado de quem abriu o wintop (processo criado com
    // CREATE_NEW_PROCESS_GROUP ou que ignorava Ctrl+C): (NULL, FALSE) restaura o padrão.
    // SAFETY: sem handler; só reativa o processamento normal do Ctrl+C neste processo.
    let _ = unsafe { SetConsoleCtrlHandler(None, false) };
    // SAFETY: função estática com a assinatura de PHANDLER_ROUTINE.
    if let Err(e) = unsafe { SetConsoleCtrlHandler(Some(on_console_event), true) } {
        eprintln!("warning: SetConsoleCtrlHandler: {e}");
    }
    let engine = engine::Engine::start(engine::DEFAULT_INTERVAL_MS);
    println!(
        "wintop {}: interval {} ms (Ctrl+C to quit)",
        env!("CARGO_PKG_VERSION"),
        engine.interval_ms()
    );
    let (mut seen, mut printed) = (0, 0);
    while !STOP.load(Ordering::SeqCst) {
        seen = engine.wait_newer(seen, Duration::from_millis(250));
        let (fast, slow, fast_seq) = engine.latest();
        // Só a coleta rápida gera linha (a lenta também acorda a espera).
        if fast_seq == printed {
            continue;
        }
        printed = fast_seq;
        let Some(f) = fast else { continue };
        let cpu = f.cpu.as_ref().ok().and_then(|c| c.usage.clone().ok());
        let ram = f.memory.as_ref().ok().and_then(|m| m.used.clone().ok());
        let gpu = f
            .gpus
            .as_ref()
            .ok()
            .and_then(|g| g.first())
            .and_then(|g| g.usage.clone().ok());
        let pcie = slow
            .and_then(|s| s.gpus.ok())
            .and_then(|g| g.into_iter().next())
            .and_then(|g| g.pcie_rx.ok());
        let fps = engine.fps().map_or_else(
            || "--".to_string(),
            |s| match s.game {
                Ok(g) => format!(
                    "{} {}{}",
                    g.name,
                    g.fps.map_or("--".into(), |v| fmt::num(v, 0)),
                    if g.approx { " (approx.)" } else { "" }
                ),
                Err(e) => e.to_string(),
            },
        );
        let own = f.own.as_ref().ok().map_or_else(
            || "--".to_string(),
            |o| {
                format!(
                    "{}% {}",
                    o.cpu.as_ref().map_or("--".into(), |v| fmt::num(*v, 3)),
                    o.private_ws
                        .as_ref()
                        .map_or("--".into(), |v| fmt::bytes(*v))
                )
            },
        );
        println!(
            "CPU {:>6}  RAM {:>9}  GPU {:>4}  PCIe RX {:>10}  collection {:.1} ms  FPS {fps}  wintop {own}",
            cpu.map_or("--".into(), fmt::pct),
            ram.map_or("--".into(), fmt::bytes),
            gpu.map_or("--".into(), |v| format!("{v}%")),
            pcie.map_or("--".into(), |v| format!("{}/s", fmt::bytes(v))),
            f.took.as_secs_f64() * 1e3
        );
    }
}
