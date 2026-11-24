//! wintop: monitor de sistema em terminal para Windows.
//!
//! Sem argumentos: o motor (threads rápida, lenta e do RTSS) e a TUI. `--check`: coleta duas
//! vezes e mostra o estado de cada métrica (só diagnostica; roda com a TUI aberta). Ver
//! `docs/metricas.md`.

mod check;
mod collect;
mod config;
mod debug;
mod engine;
mod fmt;
#[cfg(test)]
mod fuzz;
mod i18n;
mod install;
mod instance;
mod model;
mod pdh;
mod shm;
mod ui;
mod update;
mod userfile;
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
/// (as threads de coleta terminam e os handles são fechados) e sai de `run`.
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

  (no option)   open the monitor (+/- change the refresh interval, q quits); the
                first run of a copy that is not installed asks whether to install
                it or run it portable from its folder
  --check       collect twice and show the status of every metric
                (--check --json: the same report as a JSON object)
  --install     copy to Program Files and add a Start menu shortcut that opens
                wintop elevated without the UAC prompt (scheduled task)
  --uninstall   remove the scheduled task, the shortcut and the installed copy
  --version     print the version
  --help        show this help";

pub fn run() {
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
        Some(cmd @ ("--install" | "--uninstall")) => {
            let r = if cmd == "--install" {
                install::install()
            } else {
                install::uninstall()
            };
            if let Err(e) = r {
                eprintln!("wintop {cmd}: {e}");
                std::process::exit(1);
            }
            return;
        }
        Some("--check") | None => {}
        Some(other) => {
            eprintln!("unknown option: {other}\n\n{HELP}");
            std::process::exit(2);
        }
    }

    // Cópia portátil: o config fica ao lado do exe (vale também para o --check).
    let mode = update::mode();
    if let update::Mode::Portable(d) = &mode {
        config::set_portable(d.clone());
    }
    // O --check só diagnostica (não cria o pipe nem inicia o RTSS): roda com a TUI aberta.
    if arg.is_some() {
        let json = match std::env::args().nth(2).as_deref() {
            None => false,
            Some("--json") => true,
            Some(other) => {
                eprintln!("unknown option for --check: {other}\n\n{HELP}");
                std::process::exit(2);
            }
        };
        std::process::exit(check::run(json));
    }
    // Instância única antes da pergunta da primeira abertura: com outro wintop aberto, nada é
    // perguntado (a escolha se perderia, e a instalação abriria uma cópia que fecharia na hora).
    let instance = match instance::acquire() {
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
    // Primeira abertura de uma cópia que não está instalada nem marcada como portátil: pergunta
    // (só com um console de verdade; com a entrada redirecionada, segue como antes, com o config
    // em %APPDATA%).
    let mode = match mode {
        update::Mode::Undecided(dir) if std::io::IsTerminal::is_terminal(&std::io::stdin()) => {
            match update::ask_first_run(&dir) {
                update::Choice::Quit => return,
                update::Choice::Install => {
                    // A cópia instalada abre pela tarefa e precisa da instância livre.
                    drop(instance);
                    first_install();
                    return;
                }
                update::Choice::Portable => {
                    config::set_portable(dir.clone());
                    update::Mode::Portable(dir)
                }
            }
        }
        m => m,
    };
    // Sobras de uma troca anterior (o exe antigo, que estava em uso).
    update::remove_stale_old(&mode);

    if run_ui(&mode) == ui::Outcome::Restart {
        // Versão nova no lugar: a instância é liberada e o exe novo abre neste mesmo console
        // (herda a elevação). Este processo só espera por ele, para quem abriu o wintop (um
        // shell) não retomar o console no meio.
        drop(instance);
        let Some(exe) = update::exe_of(&mode) else {
            std::process::exit(1);
        };
        match update::relaunch(&exe) {
            Ok(s) => std::process::exit(s.code().unwrap_or(0)),
            Err(e) => {
                eprintln!("wintop: could not start {}: {e}", exe.display());
                std::process::exit(1);
            }
        }
    }
}

/// "Install" na primeira abertura: `--install` e abre a cópia instalada pela tarefa (elevada,
/// sem UAC). Erros ficam na tela até Enter (a janela aberta pelo duplo clique fecharia junto).
fn first_install() {
    println!();
    if let Err(e) = install::install() {
        eprintln!("wintop: install failed: {e}");
        update::pause();
        std::process::exit(1);
    }
    match install::run_task() {
        Ok(()) => println!(
            "
Opening the installed copy (Start menu: wintop)..."
        ),
        Err(e) => {
            eprintln!("wintop: installed, but could not open it: {e}");
            update::pause();
        }
    }
}

/// Interface: motor + TUI até `q`, Esc, Ctrl+C, Ctrl+Break ou fechar a janela (ou até a
/// atualização aceita ser aplicada: `Outcome::Restart`).
fn run_ui(mode: &update::Mode) -> ui::Outcome {
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
        return ui::Outcome::Quit;
    }
    // O motor é iniciado dentro da TUI, depois do terminal e do panic hook prontos, e parado
    // antes de o terminal ser restaurado.
    // Configuração do usuário: intervalo, tema, pool e ignorados do RTSS (erro = padrões e aviso
    // na tela; nunca impede de abrir).
    let cfg = config::load_home(true);
    debug::set_enabled(cfg.config.debug);
    for w in &cfg.warnings {
        debug::log("config", w);
    }
    ui::theme::set(ui::theme::build(&cfg.config.theme, &cfg.config.colors));
    let interval = cfg.config.interval_ms;
    let ignored = cfg.config.rtss_ignored.clone();
    // Versão nova: consultada numa thread (a TUI pergunta quando a resposta chegar).
    let updates = cfg.config.check_updates.then(|| ui::UpdateCtx {
        rx: update::spawn_check(),
        mode: mode.clone(),
    });
    let result = ui::run(
        move || engine::Engine::start_with(interval, ignored),
        &STOP,
        cfg,
        updates.as_ref(),
    );
    DONE.store(true, Ordering::SeqCst);
    match result {
        Ok(o) => o,
        Err(e) => {
            eprintln!("wintop: terminal error: {e}");
            std::process::exit(1);
        }
    }
}
