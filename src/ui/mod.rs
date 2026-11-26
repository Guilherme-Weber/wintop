//! Interface em terminal (ratatui + crossterm).
//!
//! Redesenha só quando há dado novo, tecla ou mudança de tamanho. `ratatui::init` liga o modo
//! raw e a tela alternativa e instala um panic hook que restaura o terminal.

pub mod theme;
mod view;
mod widgets;

#[cfg(test)]
mod tests;

use std::io::IsTerminal;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant, SystemTime};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::engine::{Engine, STEP_MS, Seen, clamp_interval};

/// Como a interface terminou.
#[derive(Debug)]
pub enum Outcome {
    Quit,
    /// Atualização aplicada: reabrir o exe novo.
    Restart(crate::update::PendingRestart),
}

/// Consulta de versão nova em andamento (`update::spawn_check`) e o modo da cópia em uso.
pub struct UpdateCtx {
    pub rx: Receiver<String>,
    pub mode: crate::update::Mode,
}

type PanicHook = Arc<dyn Fn(&std::panic::PanicHookInfo<'_>) + Sync + Send + 'static>;

/// Restaura o panic hook original ao sair de `ui::run` (inclusive se `try_init` falhar).
struct PanicHookGuard(Option<PanicHook>);

impl Drop for PanicHookGuard {
    fn drop(&mut self) {
        if let Some(orig) = self.0.take() {
            std::panic::set_hook(Box::new(move |info| orig(info)));
        }
    }
}

/// Roda até `q`, Esc, Ctrl+C ou `stop` (Ctrl+Break / fechar a janela, pelo handler do console).
/// `start` inicia o motor depois do terminal e do panic hook (um pânico precoce de coletor não
/// desfaz o terminal); o motor é parado antes de o terminal ser restaurado.
pub fn run(
    start: impl FnOnce() -> Engine,
    stop: &AtomicBool,
    mut cfg: crate::config::Loaded,
    updates: Option<&UpdateCtx>,
) -> std::io::Result<Outcome> {
    if stop.load(Ordering::SeqCst) {
        return Ok(Outcome::Quit);
    }
    // Antes da tela alternativa: o tamanho novo da janela já vale no primeiro desenho.
    if cfg.config.start_maximized {
        crate::win::maximize_classic_console();
    }
    if std::io::stdout().is_terminal() {
        let _ = ratatui::crossterm::execute!(
            std::io::stdout(),
            ratatui::crossterm::terminal::SetTitle("wintop")
        );
    }
    // Guarda o hook original para restaurá-lo na saída normal (sem empilhar wrappers).
    let orig_hook: PanicHook = Arc::from(std::panic::take_hook());
    let orig_for_ratatui = Arc::clone(&orig_hook);
    std::panic::set_hook(Box::new(move |info| orig_for_ratatui(info)));
    let _hook_guard = PanicHookGuard(Some(orig_hook));

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
    let result = event_loop(&mut terminal, &engine, stop, &mut cfg, updates);
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

/// Ações com efeito externo pedidas pelo controlador puro da interface.
#[derive(Debug, PartialEq)]
enum Effect {
    Quit,
    SetInterval(u64),
    SetView(crate::model::View),
    SetTheme {
        name: String,
        colors: Vec<(String, String)>,
        honor_no_color: bool,
    },
    SetMouse(bool),
    SetDebug(bool),
    SetLanguage(crate::i18n::Lang),
    SetRtssIgnored(Vec<String>),
    SaveConfig,
    EndProcess {
        pid: u32,
        name: String,
        create: u64,
    },
    RunRuntimes(view::RuntimeJob),
    StartUpdate(String),
    LogConfigWarnings(Vec<String>),
}

/// Abre a oferta de atualização somente quando nenhum outro modal captura o teclado.
fn on_update_offer(st: &mut view::State, version: String) -> bool {
    if matches!(st.modal, view::Modal::None) {
        st.modal = view::Modal::Update(view::UpdateUi::Offer(version));
        true
    } else {
        false
    }
}

/// Controlador puro de teclado: altera `st` e devolve `(redesenhar, efeitos)`.
fn handle_key(
    st: &mut view::State,
    k: KeyEvent,
    colors: &[(String, String)],
    can_update: bool,
) -> (bool, Vec<Effect>) {
    if k.kind != KeyEventKind::Press {
        return (false, Vec::new());
    }
    let mut effects = Vec::new();
    let ctrl_c = lower(k.code) == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL);
    match &st.modal {
        // Digitando o filtro da aba Processos/GPU: as teclas vão para o texto.
        view::Modal::Filter => {
            match k.code {
                KeyCode::Esc => {
                    st.filter.clear();
                    st.modal = view::Modal::None;
                }
                KeyCode::Enter => st.modal = view::Modal::None,
                KeyCode::Backspace => {
                    st.filter.pop();
                }
                _ if ctrl_c => effects.push(Effect::Quit),
                KeyCode::Char(c) if !c.is_control() && st.filter.chars().count() < 64 => {
                    st.filter.push(c);
                }
                _ => {}
            }
            return (true, effects);
        }
        // Versão nova: `y` baixa, confere e troca (depois reabre); qualquer outra tecla deixa
        // para a próxima abertura. Aplicando, só Ctrl+C sai.
        view::Modal::Update(u) => {
            if ctrl_c {
                effects.push(Effect::Quit);
                return (true, effects);
            }
            if let view::UpdateUi::Offer(v) = u {
                if lower(k.code) == KeyCode::Char('y') && can_update {
                    let version = v.clone();
                    st.modal = view::Modal::Update(view::UpdateUi::Applying(version.clone()));
                    effects.push(Effect::StartUpdate(version));
                } else {
                    st.modal = view::Modal::None;
                }
            }
            return (true, effects);
        }
        // Confirmação de encerrar processo: só `y` encerra; qualquer outra tecla cancela.
        view::Modal::ConfirmEnd { .. } => {
            if ctrl_c {
                effects.push(Effect::Quit);
                return (true, effects);
            }
            if let view::Modal::ConfirmEnd { pid, name, create } = std::mem::take(&mut st.modal)
                && lower(k.code) == KeyCode::Char('y')
            {
                effects.push(Effect::EndProcess { pid, name, create });
            }
            return (true, effects);
        }
        // Ajuda aberta: qualquer tecla fecha (Ctrl+C ainda sai).
        view::Modal::Help => {
            if ctrl_c {
                effects.push(Effect::Quit);
            } else {
                st.modal = view::Modal::None;
            }
            return (true, effects);
        }
        view::Modal::None => {}
    }

    // Atalhos de letra valem com Caps Lock ligado (o filtro, acima, guarda o texto como digitado).
    let mut dirty = true;
    match lower(k.code) {
        KeyCode::Delete if st.tab == view::Tab::Processes => {
            if let Some(p) = st.selected_proc() {
                st.modal = view::Modal::ConfirmEnd {
                    pid: p.pid,
                    name: p.name,
                    create: p.create,
                };
            } else {
                dirty = false;
            }
        }
        // Uma caixa ampliada: Esc volta ao normal antes de sair.
        KeyCode::Esc if st.zoom.is_some() && st.tab == view::Tab::Overview => {
            st.zoom = None;
        }
        KeyCode::Char('z') if st.tab == view::Tab::Overview => {
            st.zoom = view::Zoom::next(st.zoom, st.hide_game);
        }
        KeyCode::Char('f') if st.tab == view::Tab::Overview => {
            st.toggle_game();
            effects.push(Effect::SaveConfig);
        }
        KeyCode::Char('?') => {
            st.modal = view::Modal::Help;
        }
        KeyCode::Char('p' | ' ') => {
            st.paused = !st.paused;
        }
        // Esc com um filtro aplicado limpa o filtro (o segundo Esc sai).
        KeyCode::Esc
            if matches!(st.tab, view::Tab::Processes | view::Tab::Gpu) && !st.filter.is_empty() =>
        {
            st.filter.clear();
        }
        KeyCode::Char('q') | KeyCode::Esc => effects.push(Effect::Quit),
        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
            effects.push(Effect::Quit);
        }
        KeyCode::Char('+' | '=') => {
            let next = clamp_interval(st.interval_ms.saturating_add(STEP_MS));
            st.interval_ms = next;
            effects.push(Effect::SetInterval(next));
            effects.push(Effect::SaveConfig);
        }
        KeyCode::Char('-' | '_') => {
            let next = clamp_interval(st.interval_ms.saturating_sub(STEP_MS));
            st.interval_ms = next;
            effects.push(Effect::SetInterval(next));
            effects.push(Effect::SaveConfig);
        }
        KeyCode::Char('t') => {
            // Próximo tema; as cores avulsas do config continuam por cima.
            let next = theme::next(&st.theme_name);
            st.theme_name = next.to_string();
            effects.push(Effect::SetTheme {
                name: st.theme_name.clone(),
                colors: colors.to_vec(),
                honor_no_color: false,
            });
            effects.push(Effect::SaveConfig);
        }
        KeyCode::Char('r') => {
            st.reset_session();
        }
        KeyCode::Char('g') if matches!(st.tab, view::Tab::Overview | view::Tab::Gpu) => {
            st.next_gpu();
        }
        KeyCode::Char('d') => {
            // Automático → recolhe → expande → recolhe...
            st.toggle_fold_pool();
            effects.push(Effect::SaveConfig);
        }
        KeyCode::Char(c @ '1'..='4') => {
            st.set_tab(match c {
                '1' => view::Tab::Overview,
                '2' => view::Tab::Processes,
                '3' => view::Tab::Gpu,
                _ => view::Tab::Sensors,
            });
            effects.push(Effect::SetView(st.view()));
        }
        code => dirty = list_key(st, code, &mut effects),
    }
    (dirty, effects)
}

/// Controlador puro de mouse: só atua quando nenhum modal (inclusive filtro) está ativo.
fn handle_mouse(st: &mut view::State, m: event::MouseEvent) -> (bool, Vec<Effect>) {
    if !matches!(st.modal, view::Modal::None) {
        return (false, Vec::new());
    }
    let (redraw, tab_changed) = on_mouse(st, m);
    let mut effects = Vec::new();
    if tab_changed {
        effects.push(Effect::SetView(st.view()));
    }
    (redraw, effects)
}

/// Clique e rodinha: abas, linhas e cabeçalho das listas (o mesmo que as teclas fazem).
/// Devolve (redesenhar, a aba mudou).
fn on_mouse(st: &mut view::State, m: event::MouseEvent) -> (bool, bool) {
    use event::{MouseButton, MouseEventKind};
    let lists = matches!(st.tab, view::Tab::Processes | view::Tab::Gpu);
    match m.kind {
        MouseEventKind::ScrollDown | MouseEventKind::ScrollUp if st.tab == view::Tab::Sensors => {
            sensor_scroll(st, m.kind == MouseEventKind::ScrollDown, 3);
            (true, false)
        }
        MouseEventKind::ScrollDown | MouseEventKind::ScrollUp if lists => {
            let cursor = if st.tab == view::Tab::Processes {
                &mut st.proc_table.cursor
            } else {
                &mut st.gpu_table.cursor
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
                if st.tab == view::Tab::Processes {
                    st.proc_table.click_col(col);
                } else {
                    st.gpu_table.click_col(col);
                }
                return (true, false);
            }
            let b = h.body;
            if (b.y..b.y + b.height).contains(&m.row) && (b.x..b.x + b.width).contains(&m.column) {
                let i = h.offset + usize::from(m.row - b.y);
                if i < h.len {
                    if st.tab == view::Tab::Processes {
                        st.proc_table.cursor.select(i);
                    } else {
                        st.gpu_table.cursor.select(i);
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

/// Thread dedicada de gravação do `config.toml`: agrupa gravações em rajada (`+`/`-`/`t`) e
/// evita travar a interface durante `sync_all()` em discos lentos.
struct ConfigSaver {
    tx: Option<Sender<crate::config::SaveJob>>,
    rx: Receiver<(Result<(), String>, Option<SystemTime>)>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl ConfigSaver {
    fn new() -> Self {
        let (job_tx, job_rx) = std::sync::mpsc::channel::<crate::config::SaveJob>();
        let (res_tx, res_rx) = std::sync::mpsc::channel();
        let join = std::thread::Builder::new()
            .name("cfg-save".into())
            .spawn(move || {
                while let Ok(mut job) = job_rx.recv() {
                    // Se chegaram várias teclas seguidas, grava só o estado mais recente.
                    while let Ok(newer) = job_rx.try_recv() {
                        job = newer;
                    }
                    let r = job.write();
                    let mtime = r.is_ok().then(config_mtime).flatten();
                    if res_tx.send((r, mtime)).is_err() {
                        break;
                    }
                }
            })
            .ok();
        let tx = join.is_some().then_some(job_tx);
        Self {
            tx,
            rx: res_rx,
            join,
        }
    }

    fn send(
        &self,
        mut job: crate::config::SaveJob,
    ) -> Option<(Result<(), String>, Option<SystemTime>)> {
        if let Some(tx) = &self.tx {
            match tx.send(job) {
                Ok(()) => return None,
                Err(e) => job = e.0,
            }
        }
        // Fallback síncrono caso a thread não tenha iniciado.
        let r = job.write();
        let mtime = r.is_ok().then(config_mtime).flatten();
        Some((r, mtime))
    }
}

impl Drop for ConfigSaver {
    fn drop(&mut self) {
        drop(self.tx.take());
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Sincroniza os campos do estado da interface vindos do config (abertura e recarregamento).
fn sync_state_config(st: &mut view::State, cfg: &crate::config::Loaded, no_color: bool) {
    let c = &cfg.config;
    st.interval_ms = clamp_interval(c.interval_ms);
    st.fold_pool = c.fold_pool.as_option();
    st.hide_game = !c.show_game;
    if st.hide_game && st.zoom == Some(view::Zoom::Game) {
        st.zoom = None;
    }
    st.frametime_alert_ms = c.frametime_alert_ms;
    st.net_bytes = c.net_bytes;
    st.notice = cfg.warnings.first().cloned();
    st.theme_name = theme::build_with_no_color(&c.theme, &c.colors, no_color)
        .name
        .to_string();
}

/// Valores do config editado com o wintop aberto: atualiza o estado e devolve os efeitos.
fn apply_config_values(
    st: &mut view::State,
    cfg: &crate::config::Loaded,
    no_color: bool,
) -> Vec<Effect> {
    sync_state_config(st, cfg, no_color);
    let c = &cfg.config;
    let mut effects = vec![Effect::SetDebug(c.debug)];
    if !cfg.warnings.is_empty() {
        effects.push(Effect::LogConfigWarnings(cfg.warnings.clone()));
    }
    effects.extend([
        Effect::SetTheme {
            name: c.theme.clone(),
            colors: c.colors.clone(),
            honor_no_color: true,
        },
        Effect::SetInterval(st.interval_ms),
        Effect::SetLanguage(c.language),
        Effect::SetRtssIgnored(c.rtss_ignored.clone()),
        Effect::SetMouse(c.mouse),
    ]);
    effects
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    engine: &Engine,
    stop: &AtomicBool,
    cfg: &mut crate::config::Loaded,
    updates: Option<&UpdateCtx>,
) -> std::io::Result<Outcome> {
    let no_color = theme::no_color();
    let mut st = view::State::new(engine.interval_ms());
    sync_state_config(&mut st, cfg, no_color);
    st.interval_ms = engine.interval_ms();
    st.theme_name = theme::th().name.to_string();
    st.refresh_hz = crate::win::primary_refresh_hz();
    crate::i18n::set(cfg.config.language);
    // Config editado com o wintop aberto: a data do arquivo é conferida a cada 2 s.
    let mut cfg_seen = config_mtime();
    let mut cfg_checked = Instant::now();
    let saver = ConfigSaver::new();
    let (jobs_tx, jobs_rx) = std::sync::mpsc::channel::<RuntimeResult>();
    let mut seen = Seen::default();
    let mut dirty = true;
    let mut last_second = (u16::MAX, 0, 0);
    let mut applying: Option<Receiver<Result<crate::update::PendingRestart, String>>> = None;

    let mut apply_save_result = |r: Result<(), String>,
                                 mtime: Option<SystemTime>,
                                 cfg: &crate::config::Loaded,
                                 cfg_seen: &mut Option<SystemTime>,
                                 st: &mut view::State| {
        // Gravou: atualiza `cfg_seen` para o próprio salvamento não disparar reload.
        match r {
            Ok(()) => {
                if mtime.is_some() {
                    *cfg_seen = mtime;
                }
                st.notice = cfg.warnings.first().cloned();
            }
            Err(e) => {
                crate::debug::log("config", &format!("save failed: {e}"));
                st.notice = Some(e);
            }
        }
    };

    while !stop.load(Ordering::SeqCst) {
        // Versão nova: a resposta da consulta abre a pergunta (uma vez por abertura), mas só
        // quando nada mais captura o teclado.
        if let Some(u) = updates
            && matches!(st.modal, view::Modal::None)
            && let Ok(v) = u.rx.try_recv()
        {
            dirty |= on_update_offer(&mut st, v);
        }
        let done = applying.as_ref().and_then(|rx| match rx.try_recv() {
            Ok(r) => Some(r),
            Err(TryRecvError::Empty) => None,
            // A thread não chegou a rodar (ou morreu sem responder).
            Err(TryRecvError::Disconnected) => Some(Err("update thread did not start".into())),
        });
        if let Some(r) = done {
            applying = None;
            match r {
                Ok(pending) => return Ok(Outcome::Restart(pending)),
                Err(e) => {
                    crate::debug::log("update", &format!("apply failed: {e}"));
                    st.modal = view::Modal::None;
                    st.toast = Some((format!("update failed: {e}"), true, Instant::now()));
                }
            }
            dirty = true;
        }
        // Resultados de gravações assíncronas do `config.toml`.
        while let Ok((r, mtime)) = saver.rx.try_recv() {
            apply_save_result(r, mtime, cfg, &mut cfg_seen, &mut st);
            dirty = true;
        }
        // Cada redesenho também acorda o terminal (que conta no orçamento de CPU): as fontes
        // fora de fase esperam o próximo tick da coleta rápida quando a tela não depende delas.
        while let Ok((pid, name, r)) = jobs_rx.try_recv() {
            st.finish_runtimes(pid, name, r);
            dirty = true;
        }
        if cfg_checked.elapsed() >= CONFIG_CHECK {
            cfg_checked = Instant::now();
            let m = config_mtime();
            if m != cfg_seen {
                cfg_seen = m;
                // Lido pelo mesmo caminho seguro (userfile); só leitura, nunca cria.
                let new = crate::config::load_home(false);
                if new.config != cfg.config || new.warnings != cfg.warnings {
                    let effects = apply_config_values(&mut st, &new, no_color);
                    for eff in effects {
                        if let Some(outcome) = execute_effect(
                            eff,
                            &mut st,
                            engine,
                            cfg,
                            &saver,
                            &mut cfg_seen,
                            &jobs_tx,
                            updates,
                            &mut applying,
                            no_color,
                            &mut apply_save_result,
                        ) {
                            return Ok(outcome);
                        }
                    }
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
            // O número de FPS muda a cada 250 ms, mas só aparece na aba Geral (se visível).
            dirty |= overview && !st.hide_game;
        }
        // Relógio da barra de abas: o tick da coleta já redesenha a cada segundo; só com um
        // intervalo maior o relógio pede redesenho próprio.
        let now = clock();
        if now != last_second {
            last_second = now;
            st.clock = format!("{:02}:{:02}:{:02}", now.0, now.1, now.2);
            // Taxa do monitor (teto do FPS) relida a cada 10 s: muda ao trocar o modo de vídeo.
            if now.2.is_multiple_of(10) {
                st.refresh_hz = crate::win::primary_refresh_hz();
            }
            dirty |= st.interval_ms > 1000;
        }
        // Expira a mensagem curta mesmo com a tela pausada (mantém `view::draw` puro).
        dirty |= st.expire_toast(Instant::now());
        if dirty {
            terminal.draw(|f| view::draw(f, &mut st))?;
            dirty = false;
        }
        if !event::poll(poll_timeout(st.interval_ms))? {
            continue;
        }
        // Drena a fila (ex.: dezenas de Resize ao arrastar a janela) antes de redesenhar.
        loop {
            let (ev_dirty, effects) = match event::read()? {
                Event::Key(k) => handle_key(&mut st, k, &cfg.config.colors, updates.is_some()),
                Event::Resize(..) => (true, Vec::new()),
                Event::Mouse(m) => handle_mouse(&mut st, m),
                _ => (false, Vec::new()),
            };
            dirty |= ev_dirty;
            for eff in effects {
                if let Some(outcome) = execute_effect(
                    eff,
                    &mut st,
                    engine,
                    cfg,
                    &saver,
                    &mut cfg_seen,
                    &jobs_tx,
                    updates,
                    &mut applying,
                    no_color,
                    &mut apply_save_result,
                ) {
                    return Ok(outcome);
                }
            }
            if !event::poll(Duration::ZERO)? {
                break;
            }
        }
    }
    Ok(Outcome::Quit)
}

#[allow(clippy::too_many_arguments)]
fn execute_effect(
    effect: Effect,
    st: &mut view::State,
    engine: &Engine,
    cfg: &mut crate::config::Loaded,
    saver: &ConfigSaver,
    cfg_seen: &mut Option<SystemTime>,
    jobs_tx: &Sender<RuntimeResult>,
    updates: Option<&UpdateCtx>,
    applying: &mut Option<Receiver<Result<crate::update::PendingRestart, String>>>,
    no_color: bool,
    apply_save_result: &mut impl FnMut(
        Result<(), String>,
        Option<SystemTime>,
        &crate::config::Loaded,
        &mut Option<SystemTime>,
        &mut view::State,
    ),
) -> Option<Outcome> {
    match effect {
        Effect::Quit => return Some(Outcome::Quit),
        Effect::SetInterval(ms) => {
            st.interval_ms = engine.set_interval_ms(ms);
        }
        Effect::SetView(v) => engine.set_view(v),
        Effect::SetTheme {
            name,
            colors,
            honor_no_color,
        } => {
            let t = theme::build_with_no_color(&name, &colors, honor_no_color && no_color);
            theme::set(t);
            st.theme_name = t.name.to_string();
        }
        Effect::SetMouse(on) => set_mouse(on),
        Effect::SetDebug(on) => crate::debug::set_enabled(on),
        Effect::SetLanguage(lang) => crate::i18n::set(lang),
        Effect::SetRtssIgnored(list) => engine.set_rtss_ignored(list),
        Effect::LogConfigWarnings(warnings) => {
            for w in warnings {
                crate::debug::log("config", &w);
            }
        }
        Effect::SaveConfig => {
            queue_save(cfg, st, saver, cfg_seen, apply_save_result);
        }
        Effect::EndProcess { pid, name, create } => {
            let r = crate::collect::procs::end_process(pid, &name, create);
            let (msg, err) = match r {
                Ok(()) => (format!("ended {name} (PID {pid})"), false),
                Err(e) => (e, true),
            };
            st.toast = Some((msg, err, Instant::now()));
        }
        Effect::RunRuntimes(job) => {
            let (pid, name) = (job.pid, job.name.clone());
            let tx = jobs_tx.clone();
            // Thread curta por pedido (sob demanda, raro): a interface segue respondendo.
            let spawned = std::thread::Builder::new()
                .name("runtimes".into())
                .spawn(move || {
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job.run()))
                        .unwrap_or_else(|_| crate::model::fail("module reader panicked"));
                    let _ = tx.send((job.pid, job.name, r));
                });
            if let Err(e) = spawned {
                st.finish_runtimes(pid, name, crate::model::fail(e.to_string()));
            }
        }
        Effect::StartUpdate(version) => {
            if let Some(u) = updates {
                *applying = Some(crate::update::spawn_apply(version, u.mode.clone()));
            }
        }
    }
    None
}

/// Tecla de atalho em minúscula (Caps Lock ligado não desliga `q`, `t`, `j`/`k`, Ctrl+C...).
fn lower(code: KeyCode) -> KeyCode {
    match code {
        KeyCode::Char(c) => KeyCode::Char(c.to_ascii_lowercase()),
        c => c,
    }
}

const CONFIG_CHECK: Duration = Duration::from_secs(2);

/// Data de modificação do `config.toml` sem seguir symlinks (a leitura em si passa pelo
/// `userfile`).
fn config_mtime() -> Option<SystemTime> {
    let meta = std::fs::symlink_metadata(crate::config::default_path()?).ok()?;
    if !meta.file_type().is_file() {
        return None;
    }
    meta.modified().ok()
}

/// Prepara em memória e enfileira a gravação no `config.toml` (intervalo, tema, pool); erro
/// vira aviso sem travar a interface.
fn queue_save(
    cfg: &mut crate::config::Loaded,
    st: &mut view::State,
    saver: &ConfigSaver,
    cfg_seen: &mut Option<SystemTime>,
    apply_save_result: &mut impl FnMut(
        Result<(), String>,
        Option<SystemTime>,
        &crate::config::Loaded,
        &mut Option<SystemTime>,
        &mut view::State,
    ),
) {
    let mut c = cfg.config.clone();
    c.interval_ms = st.interval_ms;
    c.theme.clone_from(&st.theme_name);
    c.fold_pool = crate::config::FoldPool::from_option(st.fold_pool);
    c.show_game = !st.hide_game;
    if c == cfg.config || !cfg.writable() {
        // Sem gravação: o aviso de leitura (que explica o porquê) continua na tela.
        return;
    }
    match cfg.prepare_save(&c) {
        Ok(job) => {
            if let Some((r, mtime)) = saver.send(job) {
                apply_save_result(r, mtime, cfg, cfg_seen, st);
            }
        }
        Err(e) => {
            crate::debug::log("config", &format!("save failed: {e}"));
            st.notice = Some(e);
        }
    }
}

/// Resultado de uma leitura de runtimes: (PID, nome, runtimes).
type RuntimeResult = (u32, String, crate::model::Metric<Vec<&'static str>>);

/// Rolagem da aba Sensores (só para baixo enquanto houver linhas de fora).
fn sensor_scroll(st: &mut view::State, down: bool, n: usize) {
    if down {
        if st.sensor_overflow {
            st.sensor_offset += n;
        }
    } else {
        st.sensor_offset = st.sensor_offset.saturating_sub(n);
    }
}

/// Teclas das listas (abas Processos, GPU e Sensores). `true` = redesenhar.
fn list_key(st: &mut view::State, code: KeyCode, effects: &mut Vec<Effect>) -> bool {
    if st.tab == view::Tab::Sensors {
        match code {
            KeyCode::Up | KeyCode::Char('k') => sensor_scroll(st, false, 1),
            KeyCode::Down | KeyCode::Char('j') => sensor_scroll(st, true, 1),
            KeyCode::PageUp => sensor_scroll(st, false, 10),
            KeyCode::PageDown => sensor_scroll(st, true, 10),
            KeyCode::Home => st.sensor_offset = 0,
            _ => return false,
        }
        return true;
    }
    let cursor = match st.tab {
        view::Tab::Processes => &mut st.proc_table.cursor,
        view::Tab::Gpu => &mut st.gpu_table.cursor,
        _ => return false,
    };
    match code {
        KeyCode::Up | KeyCode::Char('k') => cursor.up(1),
        KeyCode::Down | KeyCode::Char('j') => cursor.down(1),
        KeyCode::PageUp => cursor.page_up(),
        KeyCode::PageDown => cursor.page_down(),
        KeyCode::Home => cursor.home(),
        KeyCode::End => cursor.end(),
        KeyCode::Left if st.tab == view::Tab::Processes => st.proc_table.prev_sort(),
        KeyCode::Right if st.tab == view::Tab::Processes => st.proc_table.next_sort(),
        KeyCode::Char('i') if st.tab == view::Tab::Processes => st.proc_table.invert(),
        KeyCode::Char('e') if st.tab == view::Tab::Processes => st.proc_tree = !st.proc_tree,
        KeyCode::Left if st.tab == view::Tab::Gpu => st.gpu_table.prev_sort(),
        KeyCode::Right if st.tab == view::Tab::Gpu => st.gpu_table.next_sort(),
        KeyCode::Char('i') if st.tab == view::Tab::Gpu => st.gpu_table.invert(),
        // O filtro (nome ou PID) vale para as listas das duas abas.
        KeyCode::Char('/') if matches!(st.tab, view::Tab::Processes | view::Tab::Gpu) => {
            st.modal = view::Modal::Filter;
        }
        // Sob demanda: abre o processo selecionado (nunca o jogo nem apps com o hook do RTSS,
        // recusados antes de abrir).
        KeyCode::Enter if st.tab == view::Tab::Gpu => {
            if let Some(job) = st.request_runtimes() {
                effects.push(Effect::RunRuntimes(job));
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
mod controller_tests {
    use super::*;
    use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    #[test]
    fn espera_acompanha_o_intervalo() {
        assert_eq!(poll_timeout(100), Duration::from_millis(50));
        assert_eq!(poll_timeout(1000), Duration::from_millis(100));
        assert_eq!(poll_timeout(10), Duration::from_millis(25));
    }

    #[test]
    fn modal_exclusivo_esc_em_camadas_e_caps_lock() {
        let mut st = view::State::new(1000);
        st.theme_name = "default".into();

        // Zoom na aba Geral: Esc desfaz o zoom antes de sair.
        let (dirty, eff) = handle_key(&mut st, key(KeyCode::Char('Z')), &[], false);
        assert!(dirty && eff.is_empty() && st.zoom == Some(view::Zoom::Cpu));
        let (dirty, eff) = handle_key(&mut st, key(KeyCode::Esc), &[], false);
        assert!(dirty && eff.is_empty() && st.zoom.is_none());

        // Ajuda abre e fecha em qualquer tecla; Ctrl+C ainda sai.
        let (dirty, eff) = handle_key(&mut st, key(KeyCode::Char('?')), &[], false);
        assert!(dirty && eff.is_empty() && st.modal == view::Modal::Help);
        assert!(!on_update_offer(&mut st, "2.0.0".into()));
        let (dirty, eff) = handle_key(&mut st, key(KeyCode::Char('x')), &[], false);
        assert!(dirty && eff.is_empty() && st.modal == view::Modal::None);

        // Oferta de atualização abre com modal livre; `y` inicia atualização; Ctrl+C sai.
        assert!(on_update_offer(&mut st, "2.0.0".into()));
        let (_, eff) = handle_key(&mut st, key(KeyCode::Char('Y')), &[], true);
        assert_eq!(eff, vec![Effect::StartUpdate("2.0.0".into())]);
        assert_eq!(
            st.modal,
            view::Modal::Update(view::UpdateUi::Applying("2.0.0".into()))
        );
        // Enquanto aplica, tecla comum não fecha; Ctrl+C sai.
        let (_, eff) = handle_key(&mut st, key(KeyCode::Esc), &[], true);
        assert!(eff.is_empty());
        let (_, eff) = handle_key(&mut st, ctrl(KeyCode::Char('C')), &[], true);
        assert_eq!(eff, vec![Effect::Quit]);

        // Aba Processos + filtro: `/` abre Modal::Filter, texto mantém maiúsculas, Enter fecha,
        // 1º Esc limpa o filtro, 2º Esc sai.
        st.modal = view::Modal::None;
        let (_, eff) = handle_key(&mut st, key(KeyCode::Char('2')), &[], false);
        assert_eq!(st.tab, view::Tab::Processes);
        assert_eq!(
            eff,
            vec![Effect::SetView(crate::model::View {
                procs: true,
                gpu: false
            })]
        );
        let _ = handle_key(&mut st, key(KeyCode::Char('/')), &[], false);
        assert!(st.is_filtering());
        let _ = handle_key(&mut st, key(KeyCode::Char('A')), &[], false);
        let _ = handle_key(&mut st, key(KeyCode::Char('b')), &[], false);
        assert_eq!(st.filter, "Ab");
        let _ = handle_key(&mut st, key(KeyCode::Enter), &[], false);
        assert_eq!(st.modal, view::Modal::None);
        assert_eq!(st.filter, "Ab");
        let (_, eff) = handle_key(&mut st, key(KeyCode::Esc), &[], false);
        assert!(eff.is_empty() && st.filter.is_empty());
        let (_, eff) = handle_key(&mut st, key(KeyCode::Esc), &[], false);
        assert_eq!(eff, vec![Effect::Quit]);

        // Caps Lock em atalhos: `+`, `-`, `T`, `P`, `Q`.
        let (_, eff) = handle_key(&mut st, key(KeyCode::Char('+')), &[], false);
        assert_eq!(st.interval_ms, 1100);
        assert_eq!(eff, vec![Effect::SetInterval(1100), Effect::SaveConfig]);
        let (_, eff) = handle_key(&mut st, key(KeyCode::Char('P')), &[], false);
        assert!(eff.is_empty() && st.paused);
        let (_, eff) = handle_key(&mut st, key(KeyCode::Char('Q')), &[], false);
        assert_eq!(eff, vec![Effect::Quit]);
    }

    #[test]
    fn mouse_ignorado_durante_filtro_e_modais() {
        let mut st = view::State::new(1000);
        st.set_tab(view::Tab::Processes);
        st.hits.tabs.push((0, 10, view::Tab::Overview));
        let click_tab = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        for modal in [
            view::Modal::Filter,
            view::Modal::Help,
            view::Modal::ConfirmEnd {
                pid: 1,
                name: "x.exe".into(),
                create: 10,
            },
            view::Modal::Update(view::UpdateUi::Offer("2.0.0".into())),
        ] {
            st.modal = modal;
            let (dirty, eff) = handle_mouse(&mut st, click_tab);
            assert!(!dirty && eff.is_empty());
            assert_eq!(st.tab, view::Tab::Processes);
        }
        // Sem modal, o mesmo clique troca de aba e pede `SetView`.
        st.modal = view::Modal::None;
        let (dirty, eff) = handle_mouse(&mut st, click_tab);
        assert!(dirty);
        assert_eq!(st.tab, view::Tab::Overview);
        assert_eq!(eff, vec![Effect::SetView(crate::model::View::default())]);
    }

    #[test]
    fn toast_expira_mesmo_com_tela_pausada() {
        let mut st = view::State::new(1000);
        st.paused = true;
        let t0 = Instant::now();
        st.toast = Some(("ended x.exe (PID 1)".into(), false, t0));
        assert!(!st.expire_toast(t0 + view::TOAST_FOR - Duration::from_millis(1)));
        assert!(st.toast.is_some());
        assert!(st.expire_toast(t0 + view::TOAST_FOR));
        assert!(st.toast.is_none());
    }

    #[test]
    fn atalho_f_oculta_jogo_salva_config_e_pula_zoom_do_jogo() {
        let mut st = view::State::new(1000);
        assert!(!st.hide_game);
        // Com zoom no jogo, `f` oculta, sai do zoom e pede SaveConfig.
        st.zoom = Some(view::Zoom::Game);
        let (dirty, eff) = handle_key(&mut st, key(KeyCode::Char('F')), &[], false);
        assert!(dirty && st.hide_game && st.zoom.is_none());
        assert_eq!(eff, vec![Effect::SaveConfig]);
        // Com o jogo oculto, o ciclo de `z` vai até Disks e volta a None (pula Game).
        for want in [
            Some(view::Zoom::Cpu),
            Some(view::Zoom::Gpu),
            Some(view::Zoom::Net),
            Some(view::Zoom::Disks),
            None,
        ] {
            let _ = handle_key(&mut st, key(KeyCode::Char('z')), &[], false);
            assert_eq!(st.zoom, want);
        }
        // `f` de novo reexibe o jogo.
        let (dirty, eff) = handle_key(&mut st, key(KeyCode::Char('f')), &[], false);
        assert!(dirty && !st.hide_game);
        assert_eq!(eff, vec![Effect::SaveConfig]);
        // Em outra aba, `f` não altera `hide_game`.
        st.set_tab(view::Tab::Processes);
        let (dirty, eff) = handle_key(&mut st, key(KeyCode::Char('f')), &[], false);
        assert!(!dirty && eff.is_empty() && !st.hide_game);
    }
}
