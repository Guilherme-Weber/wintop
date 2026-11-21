//! Interface em terminal (ratatui + crossterm).
//!
//! Redesenha só quando há dado novo, tecla ou mudança de tamanho. `ratatui::init` liga o modo
//! raw e a tela alternativa e instala um panic hook que restaura o terminal.

mod view;
mod widgets;

#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

use crate::engine::{Engine, STEP_MS, Seen};

/// Roda até `q`, Esc, Ctrl+C ou `stop` (Ctrl+Break / fechar a janela, pelo handler do console).
/// `start` inicia o motor depois do terminal e do panic hook (um pânico precoce de coletor não
/// desfaz o terminal); o motor é parado antes de o terminal ser restaurado.
pub fn run(start: impl FnOnce() -> Engine, stop: &AtomicBool) -> std::io::Result<()> {
    if stop.load(Ordering::SeqCst) {
        return Ok(());
    }
    let mut terminal = ratatui::try_init()?;
    // O hook do ratatui restaura o terminal em qualquer pânico, inclusive nos que os coletores
    // já capturam (catch_unwind) e mostram como falha: aí a TUI seguiria desenhando com o
    // terminal desfeito. Só um pânico na thread da interface restaura o terminal.
    let ratatui_hook = std::panic::take_hook();
    let ui_thread = std::thread::current().id();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().id() == ui_thread {
            ratatui_hook(info);
        }
    }));
    let engine = start();
    let result = event_loop(&mut terminal, &engine, stop);
    // Threads paradas (handles fechados, PawnIO e NVML liberados) antes de restaurar.
    drop(engine);
    ratatui::restore();
    result
}

/// Espera por tecla: metade do intervalo (entre 25 e 100 ms), para não perder coletas.
fn poll_timeout(interval_ms: u64) -> Duration {
    Duration::from_millis((interval_ms / 2).clamp(25, 100))
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    engine: &Engine,
    stop: &AtomicBool,
) -> std::io::Result<()> {
    let mut st = view::State::new(engine.interval_ms());
    let mut seen = Seen::default();
    let mut dirty = true;
    while !stop.load(Ordering::SeqCst) {
        let u = engine.updates(&mut seen);
        if let Some(f) = u.fast {
            st.push_fast(f);
            dirty = true;
        }
        if let Some(s) = u.slow {
            st.slow = Some(s);
            dirty = true;
        }
        if let Some(s) = u.fps {
            st.push_fps(s);
            dirty = true;
        }
        if dirty {
            terminal.draw(|f| view::draw(f, &mut st))?;
            dirty = false;
        }
        if !event::poll(poll_timeout(st.interval_ms))? {
            continue;
        }
        // Drena a fila (ex.: dezenas de Resize ao arrastar a janela) antes de redesenhar.
        loop {
            match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press => match k.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(());
                    }
                    KeyCode::Char('+' | '=') => {
                        st.interval_ms = engine.set_interval_ms(engine.interval_ms() + STEP_MS);
                        dirty = true;
                    }
                    KeyCode::Char('-' | '_') => {
                        st.interval_ms =
                            engine.set_interval_ms(engine.interval_ms().saturating_sub(STEP_MS));
                        dirty = true;
                    }
                    _ => {}
                },
                Event::Resize(..) => dirty = true,
                _ => {}
            }
            if !event::poll(Duration::ZERO)? {
                break;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod poll_tests {
    use super::*;

    #[test]
    fn espera_acompanha_o_intervalo() {
        assert_eq!(poll_timeout(100), Duration::from_millis(50));
        assert_eq!(poll_timeout(1000), Duration::from_millis(100));
        assert_eq!(poll_timeout(10), Duration::from_millis(25));
    }
}
