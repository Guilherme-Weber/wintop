//! wintop: monitor de sistema em terminal para Windows.
//!
//! Sem argumentos: o motor (threads rápida, lenta e do RTSS) e a TUI. `--check`: coleta duas
//! vezes e mostra o estado de cada métrica (só diagnostica; roda com a TUI aberta). Ver
//! `docs/metricas.md`.

mod check;
mod collect;
mod config;
mod engine;
mod fmt;
mod instance;
mod model;
mod pdh;
mod shm;
mod ui;
mod win;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use windows::Win32::System::Console::{
    CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, SetConsoleCtrlHandler,
};
use windows::core::BOOL;

/// Pedido de parada (Ctrl+C, Ctrl+Break ou fechar a janela do console).
static STOP: AtomicBool = AtomicBool::new(false);
/// Parada concluída (motor encerrado): o handler do fechamento da janela pode retornar.
static DONE: AtomicBool = AtomicBool::new(false);
/// O Windows dá ~5 s ao handler do fechamento antes de encerrar o processo.
const CLOSE_WAIT: Duration = Duration::from_millis(4500);

/// Roda numa thread criada pelo Windows: só marca a parada; o laço principal encerra o motor
/// (as threads de coleta terminam e os handles são fechados) e sai de `main`.
unsafe extern "system" fn on_console_event(kind: u32) -> BOOL {
    if matches!(kind, CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT) {
        STOP.store(true, Ordering::SeqCst);
        // Ao fechar a janela o Windows encerra o processo quando o handler retorna: espera a
        // parada terminar de fato (motor encerrado), até o limite que o Windows concede.
        if kind == CTRL_CLOSE_EVENT {
            let t = std::time::Instant::now();
            while !DONE.load(Ordering::SeqCst) && t.elapsed() < CLOSE_WAIT {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        BOOL(1)
    } else {
        BOOL(0)
    }
}

const HELP: &str = "\
usage: wintop [option]

  (no option)   open the monitor (+/- change the refresh interval, q quits)
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

    // O --check só diagnostica (não cria o pipe nem inicia o RTSS): roda com a TUI aberta.
    if arg.is_some() {
        std::process::exit(check::run());
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

    run_ui();
}

/// Interface: motor + TUI até `q`, Esc, Ctrl+C, Ctrl+Break ou fechar a janela.
fn run_ui() {
    // Ctrl+C pode chegar desligado, herdado de quem abriu o wintop (processo criado com
    // CREATE_NEW_PROCESS_GROUP ou que ignorava Ctrl+C): (NULL, FALSE) restaura o padrão.
    // SAFETY: sem handler; só reativa o processamento normal do Ctrl+C neste processo.
    let _ = unsafe { SetConsoleCtrlHandler(None, false) };
    // SAFETY: função estática com a assinatura de PHANDLER_ROUTINE.
    if let Err(e) = unsafe { SetConsoleCtrlHandler(Some(on_console_event), true) } {
        eprintln!("warning: SetConsoleCtrlHandler: {e}");
    }
    if STOP.load(Ordering::SeqCst) {
        DONE.store(true, Ordering::SeqCst);
        return;
    }
    // O motor é iniciado dentro da TUI, depois do terminal e do panic hook prontos, e parado
    // antes de o terminal ser restaurado.
    // Configuração do usuário: intervalo, tema, pool e ignorados do RTSS (erro = padrões e aviso
    // na tela; nunca impede de abrir).
    let cfg = config::load(config::default_path());
    ui::theme::set(ui::theme::build(&cfg.config.theme, &cfg.config.colors));
    let interval = cfg.config.interval_ms;
    let ignored = cfg.config.rtss_ignored.clone();
    let result = ui::run(
        move || engine::Engine::start_with(interval, ignored),
        &STOP,
        cfg,
    );
    DONE.store(true, Ordering::SeqCst);
    if let Err(e) = result {
        eprintln!("wintop: terminal error: {e}");
        std::process::exit(1);
    }
}
