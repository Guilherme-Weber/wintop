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

use crate::engine::{Engine, STEP_MS};

/// Espera máxima por tecla antes de olhar se o motor publicou dado novo.
const POLL: Duration = Duration::from_millis(200);

/// Roda até `q`, Esc, Ctrl+C ou `stop` (Ctrl+Break / fechar a janela, pelo handler do console).
pub fn run(engine: &Engine, stop: &AtomicBool) -> std::io::Result<()> {
    let mut terminal = ratatui::try_init()?;
    let result = event_loop(&mut terminal, engine, stop);
    ratatui::restore();
    result
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    engine: &Engine,
    stop: &AtomicBool,
) -> std::io::Result<()> {
    let mut st = view::State::new(engine.interval_ms());
    let mut seen_fast = 0;
    let mut seen_fps = 0u64;
    let mut dirty = true;
    while !stop.load(Ordering::SeqCst) {
        let (fast, slow, fast_seq) = engine.latest();
        if fast_seq != seen_fast {
            seen_fast = fast_seq;
            if let Some(f) = fast {
                st.push_fast(f);
            }
            dirty = true;
        }
        if slow.is_some() {
            st.slow = slow;
        }
        let (fps, fps_seq) = engine.fps_seq();
        if fps_seq != seen_fps {
            seen_fps = fps_seq;
            if let Some(s) = fps {
                st.push_fps(s);
                dirty = true;
            }
        }
        if dirty {
            terminal.draw(|f| view::draw(f, &mut st))?;
            dirty = false;
        }
        if event::poll(POLL)? {
            match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press => match k.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => break,
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
        }
    }
    Ok(())
}
