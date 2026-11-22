//! Interface em terminal (ratatui + crossterm).
//!
//! Redesenha só quando há dado novo, tecla ou mudança de tamanho. `ratatui::init` liga o modo
//! raw e a tela alternativa e instala um panic hook que restaura o terminal.

pub mod theme;
mod view;
mod widgets;

#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::Duration;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

use crate::engine::{Engine, STEP_MS, Seen};

/// Roda até `q`, Esc, Ctrl+C ou `stop` (Ctrl+Break / fechar a janela, pelo handler do console).
/// `start` inicia o motor depois do terminal e do panic hook (um pânico precoce de coletor não
/// desfaz o terminal); o motor é parado antes de o terminal ser restaurado.
pub fn run(
    start: impl FnOnce() -> Engine,
    stop: &AtomicBool,
    mut cfg: crate::config::Loaded,
) -> std::io::Result<()> {
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
            set_mouse(false);
            ratatui_hook(info);
        }
    }));
    set_mouse(cfg.config.mouse);
    let engine = start();
    let result = event_loop(&mut terminal, &engine, stop, &mut cfg);
    // Threads paradas (handles fechados, PawnIO e NVML liberados) antes de restaurar.
    drop(engine);
    set_mouse(false);
    ratatui::restore();
    result
}

/// Liga ou desliga a captura do mouse no terminal (sempre desligada ao sair, inclusive num
/// pânico: senão o terminal fica sem seleção de texto).
fn set_mouse(on: bool) {
    use ratatui::crossterm::event::{DisableMouseCapture, EnableMouseCapture};
    use ratatui::crossterm::execute;
    let mut out = std::io::stdout();
    let _ = if on {
        execute!(out, EnableMouseCapture)
    } else {
        execute!(out, DisableMouseCapture)
    };
}

/// Clique e rodinha: abas, linhas e cabeçalho das listas (o mesmo que as teclas fazem).
/// Devolve (redesenhar, a aba mudou).
fn on_mouse(st: &mut view::State, m: event::MouseEvent) -> (bool, bool) {
    use event::{MouseButton, MouseEventKind};
    let lists = matches!(st.tab, view::Tab::Processes | view::Tab::Gpu);
    match m.kind {
        MouseEventKind::ScrollDown | MouseEventKind::ScrollUp if lists => {
            let cursor = if st.tab == view::Tab::Processes {
                &mut st.proc_cursor
            } else {
                &mut st.gpu_cursor
            };
            if m.kind == MouseEventKind::ScrollDown {
                cursor.down(3);
            } else {
                cursor.up(3);
            }
            (true, false)
        }
        MouseEventKind::Down(MouseButton::Left) => {
            if let Some(&(_, _, t)) = st
                .hits
                .tabs
                .iter()
                .find(|(a, b, _)| m.row == 0 && (*a..*b).contains(&m.column))
            {
                st.set_tab(t);
                return (true, true);
            }
            let Some(h) = &st.hits.list else {
                return (false, false);
            };
            if m.row == h.header_y {
                let Some(&(_, _, col)) =
                    h.cols.iter().find(|(a, b, _)| (*a..*b).contains(&m.column))
                else {
                    return (false, false);
                };
                // Clique na coluna da ordem inverte; em outra, ordena por ela.
                if st.tab == view::Tab::Processes {
                    if let Some(s) = view::ProcSort::from_index(col) {
                        if s == st.proc_sort {
                            st.proc_reverse = !st.proc_reverse;
                        } else {
                            st.proc_sort = s;
                            st.proc_reverse = false;
                        }
                    }
                } else if let Some(s) = view::GpuSort::from_index(col) {
                    if s == st.gpu_sort {
                        st.gpu_reverse = !st.gpu_reverse;
                    } else {
                        st.gpu_sort = s;
                        st.gpu_reverse = false;
                    }
                }
                return (true, false);
            }
            let b = h.body;
            if (b.y..b.y + b.height).contains(&m.row) && (b.x..b.x + b.width).contains(&m.column) {
                let i = h.offset + usize::from(m.row - b.y);
                if i < h.len {
                    if st.tab == view::Tab::Processes {
                        st.proc_cursor.select(i);
                    } else {
                        st.gpu_cursor.select(i);
                    }
                    return (true, false);
                }
            }
            (false, false)
        }
        _ => (false, false),
    }
}

/// Espera por tecla: metade do intervalo (entre 25 e 100 ms), para não perder coletas.
fn poll_timeout(interval_ms: u64) -> Duration {
    Duration::from_millis((interval_ms / 2).clamp(25, 100))
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    engine: &Engine,
    stop: &AtomicBool,
    cfg: &mut crate::config::Loaded,
) -> std::io::Result<()> {
    let mut st = view::State::new(engine.interval_ms());
    st.fold_pool = cfg.config.fold_pool.as_option();
    st.theme_name = theme::th().name.to_string();
    st.notice = cfg.warnings.first().cloned();
    st.frametime_alert_ms = cfg.config.frametime_alert_ms;
    st.net_bytes = cfg.config.net_bytes;
    // Config editado com o wintop aberto: a data do arquivo é conferida a cada 2 s.
    let mut cfg_seen = config_mtime();
    let mut cfg_checked = std::time::Instant::now();
    let (jobs_tx, jobs_rx) = std::sync::mpsc::channel::<RuntimeResult>();
    let mut seen = Seen::default();
    let mut dirty = true;
    let mut last_second = (u16::MAX, 0, 0);
    while !stop.load(Ordering::SeqCst) {
        // Cada redesenho também acorda o terminal (que conta no orçamento de CPU): as fontes
        // fora de fase esperam o próximo tick da coleta rápida quando a tela não depende delas.
        while let Ok((pid, name, r)) = jobs_rx.try_recv() {
            st.finish_runtimes(pid, name, r);
            dirty = true;
        }
        if cfg_checked.elapsed() >= CONFIG_CHECK {
            cfg_checked = std::time::Instant::now();
            let m = config_mtime();
            if m != cfg_seen {
                cfg_seen = m;
                // Lido pelo mesmo caminho seguro (userfile); só leitura, nunca cria.
                let new = crate::config::load(crate::config::roaming(), false);
                if new.config != cfg.config || new.warnings != cfg.warnings {
                    apply_config(&mut st, engine, &new);
                    dirty = true;
                }
                *cfg = new;
            }
        }
        let u = engine.updates(&mut seen);
        let overview = st.tab == view::Tab::Overview;
        // Pausada, a tela fica como está: as coletas continuam, mas não entram no estado.
        let u = if st.paused {
            crate::engine::Updates::default()
        } else {
            u
        };
        if let Some(f) = u.fast {
            st.push_fast(f);
            dirty = true;
        }
        if let Some(s) = u.slow {
            st.push_slow(s);
            // Na aba Geral (temperaturas, PCIe) espera o tick; nas outras é o dado da aba.
            dirty |= !overview;
        }
        if let Some(s) = u.fps {
            st.push_fps(s);
            // O número de FPS muda a cada 250 ms, mas só aparece na aba Geral.
            dirty |= overview;
        }
        // Relógio da barra de abas: o tick da coleta já redesenha a cada segundo; só com um
        // intervalo maior o relógio pede redesenho próprio.
        let now = clock();
        if now != last_second {
            last_second = now;
            st.clock = format!("{:02}:{:02}:{:02}", now.0, now.1, now.2);
            dirty |= st.interval_ms > 1000;
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
                // Digitando o filtro da aba Processos: as teclas vão para o texto.
                Event::Key(k) if k.kind == KeyEventKind::Press && st.filtering => {
                    match k.code {
                        KeyCode::Esc => {
                            st.filter.clear();
                            st.filtering = false;
                        }
                        KeyCode::Enter => st.filtering = false,
                        KeyCode::Backspace => {
                            st.filter.pop();
                        }
                        KeyCode::Char('c' | 'C') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                            return Ok(());
                        }
                        KeyCode::Char(c) if !c.is_control() && st.filter.chars().count() < 64 => {
                            st.filter.push(c);
                        }
                        _ => {}
                    }
                    dirty = true;
                }
                // Confirmação de encerrar processo: só `y` encerra; qualquer outra tecla cancela
                // (Ctrl+C ainda sai).
                Event::Key(k) if k.kind == KeyEventKind::Press && st.confirm_end.is_some() => {
                    let ctrl_c = lower(k.code) == KeyCode::Char('c')
                        && k.modifiers.contains(KeyModifiers::CONTROL);
                    if ctrl_c {
                        return Ok(());
                    }
                    if let Some((pid, name, create)) = st.confirm_end.take()
                        && lower(k.code) == KeyCode::Char('y')
                    {
                        let r = crate::collect::procs::end_process(pid, &name, create);
                        st.toast = Some(match r {
                            Ok(()) => (format!("ended {name} (PID {pid})"), false),
                            Err(e) => (e, true),
                        })
                        .map(|(m, e)| (m, e, std::time::Instant::now()));
                    }
                    dirty = true;
                }
                // Ajuda aberta: qualquer tecla fecha (Ctrl+C ainda sai).
                Event::Key(k) if k.kind == KeyEventKind::Press && st.help => {
                    if lower(k.code) == KeyCode::Char('c')
                        && k.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        return Ok(());
                    }
                    st.help = false;
                    dirty = true;
                }
                // Atalhos de letra valem com Caps Lock ligado (o filtro, acima, guarda o texto
                // como digitado).
                Event::Key(k) if k.kind == KeyEventKind::Press => match lower(k.code) {
                    KeyCode::Delete if st.tab == view::Tab::Processes => {
                        if let Some(p) = st.selected_proc() {
                            st.confirm_end = Some((p.pid, p.name, p.create));
                            dirty = true;
                        }
                    }
                    KeyCode::Char('?') => {
                        st.help = true;
                        dirty = true;
                    }
                    KeyCode::Char('p' | ' ') => {
                        st.paused = !st.paused;
                        dirty = true;
                    }
                    // Esc com um filtro aplicado limpa o filtro (o segundo Esc sai).
                    KeyCode::Esc
                        if matches!(st.tab, view::Tab::Processes | view::Tab::Gpu)
                            && !st.filter.is_empty() =>
                    {
                        st.filter.clear();
                        dirty = true;
                    }
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(());
                    }
                    KeyCode::Char('+' | '=') => {
                        st.interval_ms = engine.set_interval_ms(engine.interval_ms() + STEP_MS);
                        save(cfg, &mut st);
                        dirty = true;
                    }
                    KeyCode::Char('-' | '_') => {
                        st.interval_ms =
                            engine.set_interval_ms(engine.interval_ms().saturating_sub(STEP_MS));
                        save(cfg, &mut st);
                        dirty = true;
                    }
                    KeyCode::Char('t') => {
                        // Próximo tema; as cores avulsas do config continuam por cima.
                        let next = theme::next(&st.theme_name);
                        theme::set(theme::build(next, &cfg.config.colors));
                        st.theme_name = next.to_string();
                        save(cfg, &mut st);
                        dirty = true;
                    }
                    KeyCode::Char('r') => {
                        st.reset_session();
                        dirty = true;
                    }
                    KeyCode::Char('d') => {
                        // Automático → recolhe → expande → recolhe...
                        st.toggle_fold_pool();
                        save(cfg, &mut st);
                        dirty = true;
                    }
                    KeyCode::Char(c @ '1'..='4') => {
                        st.set_tab(match c {
                            '1' => view::Tab::Overview,
                            '2' => view::Tab::Processes,
                            '3' => view::Tab::Gpu,
                            _ => view::Tab::Sensors,
                        });
                        engine.set_view(st.view());
                        dirty = true;
                    }
                    code => dirty |= list_key(&mut st, code, &jobs_tx),
                },
                Event::Resize(..) => dirty = true,
                Event::Mouse(m) if !st.help && st.confirm_end.is_none() => {
                    let (redraw, tab_changed) = on_mouse(&mut st, m);
                    if tab_changed {
                        engine.set_view(st.view());
                    }
                    dirty |= redraw;
                }
                _ => {}
            }
            if !event::poll(Duration::ZERO)? {
                break;
            }
        }
    }
    Ok(())
}

/// Teclas das listas (abas Processos e GPU). `true` = redesenhar.
/// Grava no `config.toml` o que a interface muda (intervalo, tema, pool); erro vira aviso.
/// Tecla de atalho em minúscula (Caps Lock ligado não desliga `q`, `t`, `j`/`k`, Ctrl+C...).
fn lower(code: KeyCode) -> KeyCode {
    match code {
        KeyCode::Char(c) => KeyCode::Char(c.to_ascii_lowercase()),
        c => c,
    }
}

const CONFIG_CHECK: Duration = Duration::from_secs(2);

/// Data de modificação do `config.toml` (só para perceber a mudança; a leitura em si passa
/// pelo `userfile`).
fn config_mtime() -> Option<std::time::SystemTime> {
    std::fs::metadata(crate::config::default_path()?)
        .ok()?
        .modified()
        .ok()
}

/// Valores do config editado com o wintop aberto, aplicados na hora.
fn apply_config(st: &mut view::State, engine: &Engine, cfg: &crate::config::Loaded) {
    let c = &cfg.config;
    theme::set(theme::build(&c.theme, &c.colors));
    st.theme_name = theme::th().name.to_string();
    st.interval_ms = engine.set_interval_ms(c.interval_ms);
    st.fold_pool = c.fold_pool.as_option();
    st.frametime_alert_ms = c.frametime_alert_ms;
    st.net_bytes = c.net_bytes;
    engine.set_rtss_ignored(c.rtss_ignored.clone());
    set_mouse(c.mouse);
    st.notice = cfg.warnings.first().cloned();
}

fn save(cfg: &mut crate::config::Loaded, st: &mut view::State) {
    let mut c = cfg.config.clone();
    c.interval_ms = st.interval_ms;
    c.theme.clone_from(&st.theme_name);
    c.fold_pool = crate::config::FoldPool::from_option(st.fold_pool);
    if c == cfg.config || !cfg.writable() {
        // Sem gravação: o aviso de leitura (que explica o porquê) continua na tela.
        return;
    }
    // Gravou: um erro anterior (arquivo travado por um editor) deixa de valer.
    st.notice = match cfg.save(&c) {
        Ok(()) => cfg.warnings.first().cloned(),
        Err(e) => Some(e),
    };
}

/// Resultado de uma leitura de runtimes: (PID, nome, runtimes).
type RuntimeResult = (u32, String, crate::model::Metric<Vec<&'static str>>);

fn list_key(st: &mut view::State, code: KeyCode, jobs: &Sender<RuntimeResult>) -> bool {
    let cursor = match st.tab {
        view::Tab::Processes => &mut st.proc_cursor,
        view::Tab::Gpu => &mut st.gpu_cursor,
        _ => return false,
    };
    match code {
        KeyCode::Up | KeyCode::Char('k') => cursor.up(1),
        KeyCode::Down | KeyCode::Char('j') => cursor.down(1),
        KeyCode::PageUp => cursor.page_up(),
        KeyCode::PageDown => cursor.page_down(),
        KeyCode::Home => cursor.home(),
        KeyCode::End => cursor.end(),
        KeyCode::Left if st.tab == view::Tab::Processes => st.proc_sort = st.proc_sort.prev(),
        KeyCode::Right if st.tab == view::Tab::Processes => st.proc_sort = st.proc_sort.next(),
        KeyCode::Char('i') if st.tab == view::Tab::Processes => {
            st.proc_reverse = !st.proc_reverse;
        }
        KeyCode::Char('e') if st.tab == view::Tab::Processes => st.proc_tree = !st.proc_tree,
        KeyCode::Left if st.tab == view::Tab::Gpu => st.gpu_sort = st.gpu_sort.prev(),
        KeyCode::Right if st.tab == view::Tab::Gpu => st.gpu_sort = st.gpu_sort.next(),
        KeyCode::Char('i') if st.tab == view::Tab::Gpu => st.gpu_reverse = !st.gpu_reverse,
        // O filtro (nome ou PID) vale para as listas das duas abas.
        KeyCode::Char('/') if matches!(st.tab, view::Tab::Processes | view::Tab::Gpu) => {
            st.filtering = true;
        }
        // Sob demanda: abre o processo selecionado (nunca o jogo nem apps com o hook do RTSS,
        // recusados antes de abrir).
        KeyCode::Enter if st.tab == view::Tab::Gpu => {
            if let Some(job) = st.request_runtimes() {
                let (pid, name) = (job.pid, job.name.clone());
                let tx = jobs.clone();
                // Thread curta por pedido (sob demanda, raro): a interface segue respondendo.
                let spawned =
                    std::thread::Builder::new()
                        .name("runtimes".into())
                        .spawn(move || {
                            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                job.run()
                            }))
                            .unwrap_or_else(|_| crate::model::fail("module reader panicked"));
                            let _ = tx.send((job.pid, job.name, r));
                        });
                if let Err(e) = spawned {
                    st.finish_runtimes(pid, name, crate::model::fail(e.to_string()));
                }
            }
        }
        _ => return false,
    }
    true
}

/// Hora local (hora, minuto, segundo) para a barra de abas.
fn clock() -> (u16, u16, u16) {
    // SAFETY: sem pré-condições; devolve a estrutura por valor.
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    (t.wHour, t.wMinute, t.wSecond)
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
