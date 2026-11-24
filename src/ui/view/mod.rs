//! Aba Geral (v2, a partir do mockup da revisão visual, aprovado pelo usuário): barra de abas,
//! CPU (gráfico total + grade de gráficos por lógico) e GPU (engine mais usada, VRAM por
//! categoria) no topo; memória e rede embaixo à esquerda; discos e jogo (FPS + frametime)
//! embaixo à direita; rodapé com atalhos e o consumo do próprio wintop.
//!
//! Direção do usuário: uso em gráfico; RAM e VRAM em barra; termômetro vertical na CPU e GPU;
//! bordas neutras estilo btop; mínimo/máximo da sessão (tecla `r` zera). Desenho puro a partir
//! do estado (snapshots do `insta`).

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};

use super::theme::th;
use super::widgets::{Meter, VerticalBar};
use crate::fmt::{bytes, num};
use crate::i18n::tr;
use crate::model::{Cpu, CpuSensors, Disk, Game, Gpu, Memory, Metric, Missing, Nic, OwnUsage};
use crate::win::printable;

mod cpu;
mod disks;
mod game;
mod gpu;
mod net;
mod state;
pub(super) mod tabs;

use cpu::{draw_cpu, mem_text};
use disks::{disk_notes, disk_order, draw_disks, pool_folded, same_disk};
use game::draw_game;
pub(super) use gpu::vram_legend;
use gpu::{draw_gpu, engine_label, vram_bar};
use net::draw_net;
use state::MinMax;
pub use state::{Hits, ListHit, State, TOAST_FOR, Tab, UpdateUi, Zoom};
pub use tabs::{GpuSort, ProcSort};
// --- Texto ---

/// Valor, estado neutro (cinza) ou falha (vermelho, com a causa); sem controles.
fn metric<T>(m: &Metric<T>, f: impl FnOnce(&T) -> String) -> Span<'static> {
    metric_styled(m, th().text, f)
}

/// Como `metric`, mas pinta o valor `Ok` na cor semântica pedida (`accent`, `ok`, `warn`...).
fn metric_styled<T>(m: &Metric<T>, color: Color, f: impl FnOnce(&T) -> String) -> Span<'static> {
    match m {
        Ok(v) => Span::styled(printable(&tr(&f(v))), Style::new().fg(color)),
        Err(Missing::Neutral(s)) => Span::styled(printable(&tr(s)), Style::new().fg(th().neutral)),
        Err(Missing::Fail(s)) => Span::styled(
            format!("! {}", printable(&tr(s))),
            Style::new().fg(th().fail),
        ),
    }
}

/// Cor semântica para temperatura: normal, elevada (`warn`) perto do limite ou quente (`high`).
fn temp_color(c: f64, limit: f64) -> Color {
    if c >= limit - 10.0 {
        th().high
    } else if c >= limit - 22.0 {
        th().warn
    } else {
        th().text
    }
}

/// Largura da faixa do termômetro vertical à direita do gráfico de CPU/GPU.
const TEMP_BAR_W: u16 = 5;

/// Termômetro vertical: limite de referência no topo, barra vertical em braille com o gradiente
/// no meio e a leitura atual embaixo (enchendo de baixo para cima em direção ao limite).
fn draw_temp_bar(frame: &mut Frame<'_>, area: Rect, temp_c: f64, limit_c: f64) {
    if area.width < 4 || area.height < 4 {
        return;
    }
    let lim = label(&format!(" {:>3}°", limit_c.round() as i32));
    line(frame, vec![lim], area.x, area.y, area.width, area);
    let bar_h = area.height.saturating_sub(2);
    let val = ((temp_c - 20.0) / (limit_c - 20.0).max(1.0)).clamp(0.0, 1.0);
    let bar = Rect::new(
        area.x + 2,
        area.y + 1,
        2.min(area.width.saturating_sub(2)),
        bar_h,
    );
    frame.render_widget(VerticalBar { value: val }, bar.intersection(area));
    let cur = Span::styled(
        format!(" {:>3}°", temp_c.round() as i32),
        Style::new()
            .fg(temp_color(temp_c, limit_c))
            .add_modifier(Modifier::BOLD),
    );
    line(
        frame,
        vec![cur],
        area.x,
        area.y + area.height - 1,
        area.width,
        area,
    );
}

/// Divisor horizontal rotulado (`── memory ────`, `── VRAM ────`) dentro de uma caixa.
fn section_divider(frame: &mut Frame<'_>, divider: Rect, whole: Rect, title: &str) {
    let label_txt = format!("{} ", tr(title));
    let prefix_w = 3 + label_txt.chars().count();
    let fill = usize::from(divider.width).saturating_sub(prefix_w);
    put(
        frame,
        Paragraph::new(Line::from(vec![
            Span::styled("── ", Style::new().fg(th().border)),
            Span::styled(label_txt, Style::new().fg(th().title)),
            Span::styled("─".repeat(fill), Style::new().fg(th().border)),
        ])),
        divider,
        whole,
    );
}

fn missing(e: &Missing) -> Span<'static> {
    metric::<()>(&Err(e.clone()), |()| String::new())
}

/// Rótulo, já no idioma da interface (`i18n`).
fn label(s: &str) -> Span<'static> {
    Span::styled(tr(s).into_owned(), Style::new().fg(th().label))
}

/// Valor, já no idioma da interface (nomes e números passam intactos).
fn text(s: impl Into<String>) -> Span<'static> {
    let s: String = s.into();
    let s = match tr(&s) {
        std::borrow::Cow::Owned(t) => t,
        std::borrow::Cow::Borrowed(_) => s,
    };
    Span::styled(s, Style::new().fg(th().text))
}

/// Valor zerado em tom de rótulo (o número fica, só sai do primeiro plano).
fn faded(sp: Span<'static>, zero: bool) -> Span<'static> {
    if zero {
        sp.style(Style::new().fg(th().label))
    } else {
        sp
    }
}

fn sep() -> Span<'static> {
    Span::raw("  ")
}

fn bar_sep() -> Span<'static> {
    Span::styled("  │  ", Style::new().fg(th().dim))
}

/// "min X max Y" de um extremo da sessão.
fn minmax(m: MinMax, f: impl Fn(f64) -> String) -> Span<'static> {
    match m.get() {
        Some((lo, hi)) => label(&format!("min {} · max {}", f(lo), f(hi))),
        None => label(""),
    }
}

/// Caixa com borda neutra, título à esquerda e contexto à direita, na própria borda.
fn boxed(title: &str, right: &str) -> Block<'static> {
    boxed_ready(title, &tr(right))
}

/// Como `boxed`, com o texto da direita já no idioma da interface: o que o usuário digitou (o
/// filtro) não pode passar pela tradução ("free" viraria "livre").
fn boxed_ready(title: &str, right: &str) -> Block<'static> {
    let mut b = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(th().border))
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                printable(&tr(title)),
                Style::new().fg(th().title).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
        ]));
    if !right.is_empty() {
        b = b.title(
            Line::from(Span::styled(
                format!(" {right} "),
                Style::new().fg(th().label),
            ))
            .right_aligned(),
        );
    }
    b
}

/// Nome da caixa sem valor: a causa entra no título, com a cor do estado (vermelho na falha).
fn with_failure(b: Block<'static>, e: Option<&Missing>) -> Block<'static> {
    match e {
        Some(e) => b.title(Line::from(vec![missing(e), Span::raw(" ")])),
        None => b,
    }
}

/// Desenha um parágrafo recortado a `bound`.
/// Valor curto para um cabeçalho sem espaço para a causa: o valor, "-" (estado neutro) ou
/// "! failed" (a causa inteira vai, quebrada, nas linhas de detalhe da caixa).
fn brief<T>(m: &Metric<T>, f: impl FnOnce(&T) -> String) -> Span<'static> {
    match m {
        Ok(v) => text(f(v)),
        Err(Missing::Neutral(_)) => label("-"),
        Err(Missing::Fail(_)) => failed_span(),
    }
}

/// "! failed", no idioma da interface (mesma largura em pt-BR: "! falhou").
fn failed_span() -> Span<'static> {
    Span::styled(tr("! failed").into_owned(), Style::new().fg(th().fail))
}

fn put(frame: &mut Frame<'_>, p: Paragraph<'static>, r: Rect, bound: Rect) {
    frame.render_widget(p, r.intersection(bound));
}

fn line(frame: &mut Frame<'_>, spans: Vec<Span<'static>>, x: u16, y: u16, w: u16, bound: Rect) {
    put(
        frame,
        Paragraph::new(Line::from(spans)),
        Rect::new(x, y, w, 1),
        bound,
    );
}

/// Bytes curtos (477G, 3.1T, 859M) para colunas estreitas.
fn short_bytes(v: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut x = v as f64;
    let mut u = 0;
    while x >= 1024.0 && u < UNITS.len() - 1 {
        x /= 1024.0;
        u += 1;
    }
    if x >= 100.0 || u == 0 {
        format!("{x:.0}{}", UNITS[u])
    } else {
        format!("{}{}", num(x, 1), UNITS[u])
    }
}

fn temp_colored(m: &Metric<f64>, limit: f64) -> Span<'static> {
    let color = m.as_ref().map_or(th().text, |v| temp_color(*v, limit));
    metric_styled(m, color, |v| format!("{} °C", num(*v, 1)))
}

// --- Layout ---

/// Menor terminal em que a aba mostra tudo sem virar ruído (a Geral tem seis caixas; as outras
/// são tabelas); abaixo, o aviso honesto.
pub fn min_size(tab: Tab) -> (u16, u16) {
    match tab {
        Tab::Overview => (80, 24),
        _ => (60, 12),
    }
}

pub fn draw(frame: &mut Frame<'_>, st: &mut State) {
    st.hits = Hits::default();
    let a = frame.area();
    let (min_w, min_h) = min_size(st.tab);
    if a.width < min_w || a.height < min_h {
        let msg = vec![
            Line::from(Span::styled(
                tr("terminal too small").into_owned(),
                Style::new().fg(th().title).add_modifier(Modifier::BOLD),
            )),
            Line::from(vec![
                label(&format!("this tab needs {min_w}x{min_h}")),
                label(&format!("; the terminal is {}x{}", a.width, a.height)),
            ]),
            Line::from(label("resize the window, or press q to quit")),
        ];
        let h = (msg.len() as u16).min(a.height);
        let r = Rect::new(a.x, a.y + a.height.saturating_sub(h) / 2, a.width, h);
        frame.render_widget(Paragraph::new(msg).centered(), r);
        return;
    }
    // Aviso do config (arquivo com erro, gravação recusada): linhas próprias acima do rodapé,
    // inteiras, sem disputar espaço com o estado das fontes.
    let mut notice: Vec<Line<'static>> = st.notice.as_ref().map_or_else(Vec::new, |n| {
        wrap(
            vec![Span::styled(
                format!("! {}", printable(n)),
                Style::new().fg(th().fail),
            )],
            frame.area().width,
            1,
            3,
        )
    });
    // Resultado de uma ação (some depois de alguns segundos).
    if let Some((msg, err, at)) = &st.toast
        && at.elapsed() < TOAST_FOR
    {
        let (mark, color) = if *err {
            ("! ", th().fail)
        } else {
            ("", th().title)
        };
        notice.extend(wrap(
            vec![Span::styled(
                format!("{mark}{}", printable(&tr(msg))),
                Style::new().fg(color),
            )],
            frame.area().width,
            1,
            3,
        ));
    }
    let notice_h = (notice.len() as u16).min(frame.area().height.saturating_sub(10));
    let [tabs, body, notice_area, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(8),
        Constraint::Length(notice_h),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    if notice_h > 0 {
        frame.render_widget(Paragraph::new(notice), notice_area);
    }
    draw_tabs(frame, tabs, st);
    draw_footer(frame, footer, st);
    match st.tab {
        Tab::Overview => draw_overview(frame, body, st),
        Tab::Processes => tabs::draw_processes(frame, body, st),
        Tab::Gpu => tabs::draw_gpu_tab(frame, body, st),
        Tab::Sensors => tabs::draw_sensors(frame, body, st),
    }
    if st.help {
        draw_help(frame, body);
    }
    if let Some(u) = &st.update {
        draw_update(frame, body, u);
    }
    if let Some((pid, name, _)) = &st.confirm_end {
        draw_confirm_end(frame, body, *pid, name);
    }
}

fn draw_update(frame: &mut Frame<'_>, area: Rect, u: &UpdateUi) {
    let key = |k: &str| {
        Span::styled(
            k.to_string(),
            Style::new().fg(th().title).add_modifier(Modifier::BOLD),
        )
    };
    let ver = |v: &str| {
        Span::styled(
            format!("wintop {v}"),
            Style::new().fg(th().title).add_modifier(Modifier::BOLD),
        )
    };
    let lines = match u {
        UpdateUi::Offer(v) => vec![
            Line::from(vec![
                label(" "),
                ver(v),
                label(" is available (this is "),
                Span::raw(crate::update::current()),
                label("). Update now? "),
            ]),
            Line::from(vec![
                key(" y"),
                label(" update and restart    "),
                key("n / esc"),
                label(" not now"),
            ]),
        ],
        UpdateUi::Applying(v) => vec![
            Line::from(vec![label(" Downloading "), ver(v), label("... ")]),
            Line::from(label(" Checking the signature, then wintop restarts. ")),
        ],
    };
    let w = (lines.iter().map(|l| l.width() as u16).max().unwrap_or(0) + 3).min(area.width);
    let h = 4.min(area.height);
    let r = Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    );
    frame.render_widget(ratatui::widgets::Clear, r);
    frame.render_widget(Paragraph::new(lines).block(boxed("update", "")), r);
}

/// Confirmação de encerrar processo (por cima da aba).
fn draw_confirm_end(frame: &mut Frame<'_>, area: Rect, pid: u32, name: &str) {
    let lines = vec![
        Line::from(vec![
            label(" End "),
            Span::styled(
                format!("{} (PID {pid})", printable(name)),
                Style::new().fg(th().title).add_modifier(Modifier::BOLD),
            ),
            label("? Unsaved work in it is lost. "),
        ]),
        Line::from(vec![
            Span::styled(
                " y",
                Style::new().fg(th().fail).add_modifier(Modifier::BOLD),
            ),
            label(" end process    "),
            Span::styled(
                "n / esc",
                Style::new().fg(th().title).add_modifier(Modifier::BOLD),
            ),
            label(" cancel"),
        ]),
    ];
    let w = (lines.iter().map(|l| l.width() as u16).max().unwrap_or(0) + 3).min(area.width);
    let h = 4.min(area.height);
    let r = Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    );
    frame.render_widget(ratatui::widgets::Clear, r);
    frame.render_widget(Paragraph::new(lines).block(boxed("end process", "")), r);
}

/// Atalhos e siglas da interface (tecla `?`), numa janela por cima da aba.
const HELP: &[(&str, &str)] = &[
    ("1-4", "switch tabs (Overview, Processes, GPU, Sensors)"),
    (
        "+ / -",
        "update interval (100 ms steps, saved to config.toml)",
    ),
    ("p, space", "pause the screen (collection keeps running)"),
    (
        "z",
        "Overview: enlarge one box to the whole tab (z next, esc back)",
    ),
    ("r", "reset session min/max and the game history"),
    ("g", "next GPU, with more than one (Overview, GPU)"),
    ("t", "next color theme"),
    (
        "d",
        "fold or expand the Storage Space members in the disk box",
    ),
    ("↑↓ j k", "select a row (PgUp/PgDn, Home/End)"),
    ("←→  i", "sort column / invert order (Processes, GPU)"),
    ("/  esc", "filter by name or PID / clear the filter"),
    (
        "enter",
        "GPU tab: GPU runtimes of the selected process (on demand)",
    ),
    (
        "del",
        "Processes tab: end the selected process (asks to confirm)",
    ),
    ("e", "Processes tab: tree of parent and child processes"),
    ("q, esc", "quit (Ctrl+C also works)"),
    ("", ""),
    (
        "kern",
        "kernel (privileged) share of the CPU time, darker in the graphs",
    ),
    (
        "Tctl, CCD",
        "AMD control and per-chiplet temperatures (PawnIO)",
    ),
    ("S  P", "disk with the system / with a page file"),
    ("act, resp", "disk active time / average response time"),
    (
        "FT, lows",
        "frametime; average FPS of the slowest 1% and 0.1% frames",
    ),
    (
        "ETW, pipe",
        "per frame: ETW or RTSS pipe (exact); approx = shared-memory sampling",
    ),
    (
        "real",
        "frames rendered by the game; the rest are generated (frame gen), darker",
    ),
    (
        "red text",
        "a failed source, with its cause; gray = expected state",
    ),
];

fn draw_help(frame: &mut Frame<'_>, area: Rect) {
    let keys: Vec<String> = HELP.iter().map(|(k, _)| tr(k).into_owned()).collect();
    let key_w = keys.iter().map(|k| k.chars().count()).max().unwrap_or(0);
    let lines: Vec<Line<'static>> = keys
        .iter()
        .zip(HELP.iter().map(|(_, d)| d))
        .map(|(k, d)| {
            Line::from(vec![
                Span::styled(
                    format!(" {k:<key_w$}  "),
                    Style::new().fg(th().title).add_modifier(Modifier::BOLD),
                ),
                label(d),
            ])
        })
        .collect();
    let w = lines
        .iter()
        .map(|l| l.width() as u16)
        .max()
        .unwrap_or(0)
        .saturating_add(3)
        .min(area.width);
    let h = (lines.len() as u16 + 2).min(area.height);
    let r = Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    );
    frame.render_widget(ratatui::widgets::Clear, r);
    frame.render_widget(
        Paragraph::new(lines).block(boxed("help", "any key closes")),
        r,
    );
}

fn draw_overview(frame: &mut Frame<'_>, body: Rect, st: &mut State) {
    // Uma caixa ampliada ocupa a aba inteira.
    if let Some(z) = st.zoom {
        match z {
            Zoom::Cpu => {
                let mem_rows = mem_text(st, body.width).map_or(2, |t| t.height().saturating_sub(2));
                draw_cpu(frame, body, st, mem_rows);
            }
            Zoom::Gpu => draw_gpu(frame, body, st, None),
            Zoom::Net => draw_net(frame, body, st),
            Zoom::Disks => draw_disks(frame, body, st),
            Zoom::Game => draw_game(frame, body, st),
        }
        return;
    }
    // Em cima, CPU + memória (como GPU + VRAM ao lado); embaixo, rede compacta à esquerda e
    // discos sobre o jogo à direita.
    // Em cima, pelo menos o que CPU + memória e GPU + VRAM precisam para mostrar todas as
    // linhas de detalhe (o conhost do atalho abre com 30 linhas); os gráficos é que encolhem.
    const TOP_MIN: u16 = 23;
    // A caixa do jogo tem sempre a mesma altura, com ou sem jogo (o layout não pula quando um
    // jogo abre; pedido do usuário, 2026-11-22).
    let game_min = 8;
    // Discos: cabeçalho + uma linha por disco visível, mais uma por causa de falha (inteira,
    // embaixo da tabela). Largura da coluna da direita, como no layout abaixo.
    let right_w = body.width - body.width * 34 / 100;
    let disk_rows = (disk_order(st).len() + disk_notes(st, right_w.saturating_sub(2)).len()) as u16;
    let top_h = (body.height * 58 / 100).max(TOP_MIN);
    // Embaixo, sempre o mínimo útil: algumas linhas de discos e a faixa do jogo.
    let bottom_min = ((disk_rows.min(3) + 3 + game_min).max(9)).min(body.height / 2);
    let top_h = top_h.min(body.height.saturating_sub(bottom_min));
    let [top, bottom] =
        Layout::vertical([Constraint::Length(top_h), Constraint::Min(0)]).areas(body);
    let [cpu, gpu] =
        Layout::horizontal([Constraint::Percentage(56), Constraint::Percentage(44)]).areas(top);
    let [net, right] =
        Layout::horizontal([Constraint::Percentage(34), Constraint::Percentage(66)]).areas(bottom);
    // Linhas da memória dentro da caixa da CPU (sem as bordas, que são da caixa).
    let mem_rows = mem_text(st, cpu.width).map_or(2, |t| t.height().saturating_sub(2));
    let disks_h = (disk_rows + 3)
        .min(right.height.saturating_sub(game_min))
        .max(4);
    let [disks, game] =
        Layout::vertical([Constraint::Length(disks_h), Constraint::Min(4)]).areas(right);
    let core_graph = draw_cpu(frame, cpu, st, mem_rows);
    draw_gpu(frame, gpu, st, core_graph);
    draw_net(frame, net, st);
    draw_disks(frame, disks, st);
    draw_game(frame, game, st);
}

fn draw_tabs(frame: &mut Frame<'_>, area: Rect, st: &mut State) {
    // Estreito: abas com um espaço só entre elas (sobra para o estado à direita).
    let pad = if area.width < 100 { "" } else { " " };
    let tab = |n: &str, name: &str, t: Tab| -> Span<'static> {
        if st.tab == t {
            Span::styled(
                format!("{pad}[{n} {name}]{pad}"),
                Style::new().fg(th().title).add_modifier(Modifier::BOLD),
            )
        } else {
            label(&format!(" {pad}{n} {name}{pad} "))
        }
    };
    let spans = vec![
        tab("1", &tr("Overview"), Tab::Overview),
        tab("2", &tr("Processes"), Tab::Processes),
        tab("3", "GPU", Tab::Gpu),
        tab("4", &tr("Sensors"), Tab::Sensors),
    ];
    let mut x = area.x;
    for (sp, t) in spans
        .iter()
        .zip([Tab::Overview, Tab::Processes, Tab::Gpu, Tab::Sensors])
    {
        let w = sp.width() as u16;
        st.hits.tabs.push((x, x + w, t));
        x += w;
    }
    let tabs = Line::from(spans);
    let tw = (tabs.width() as u16).min(area.width);
    let [left, rest] = Layout::horizontal([Constraint::Length(tw), Constraint::Min(0)]).areas(area);
    frame.render_widget(Paragraph::new(tabs), left);
    let room = usize::from(rest.width.saturating_sub(1));
    let width = |v: &[Span<'static>]| v.iter().map(Span::width).sum::<usize>();
    let key = |k: &str| {
        Span::styled(
            tr(k).into_owned(),
            Style::new().fg(th().title).add_modifier(Modifier::BOLD),
        )
    };
    // O essencial primeiro (ampliação, pausa, intervalo); uptime e relógio só se couberem.
    let mut right = Vec::new();
    if let Some(z) = st.zoom
        && st.tab == Tab::Overview
    {
        right.push(key(&format!("zoom: {} ", z.name())));
        right.push(label("(z next, esc back)"));
        right.push(bar_sep());
    }
    if st.paused {
        // Destaque sem a cor de falha (vermelho é só para erro); os colchetes valem em
        // monocromático. Forma curta quando estreito.
        let long = "[PAUSED · p resumes]";
        right.push(Span::styled(
            tr(if room >= 40 { long } else { "[PAUSED]" }).into_owned(),
            Style::new()
                .fg(th().frametime)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
        ));
        right.push(bar_sep());
    }
    // Intervalo com as teclas que o mudam ao lado (como o btop).
    right.extend([
        key("- "),
        label(&format!("{} ms", st.interval_ms)),
        key(" +"),
    ]);
    let up = st
        .fast
        .as_ref()
        .and_then(|f| f.cpu.as_ref().ok())
        .map(|c| c.uptime.as_secs());
    if let Some(s) = up {
        let extra = [
            bar_sep(),
            label(&format!(
                "up {}d {:02}:{:02}:{:02}",
                s / 86400,
                s / 3600 % 24,
                s / 60 % 60,
                s % 60
            )),
        ];
        if width(&right) + width(&extra) <= room {
            right.extend(extra);
        }
    }
    if !st.clock.is_empty() {
        let extra = [bar_sep(), text(st.clock.clone())];
        if width(&right) + width(&extra) <= room {
            right.extend(extra);
        }
    }
    frame.render_widget(Paragraph::new(Line::from(right)).right_aligned(), rest);
}

fn draw_footer(frame: &mut Frame<'_>, area: Rect, st: &State) {
    let key = |k: &str| {
        Span::styled(
            tr(k).into_owned(),
            Style::new().fg(th().title).add_modifier(Modifier::BOLD),
        )
    };
    // Atalhos em grupos inteiros (tecla + rótulo): um grupo que não cabe sai inteiro.
    // Digitando o filtro, as teclas viram texto: só os atalhos do filtro (e Ctrl+C).
    let mut groups: Vec<[Span<'static>; 2]> = if st.filtering {
        Vec::new()
    } else {
        vec![[key(" 1-4"), label(" tabs  ")]]
    };
    // `d` só com Storage Space na tela, dizendo o que a tecla vai fazer agora.
    let disks = st.fast.as_ref().and_then(|f| f.disks.as_ref().ok());
    let pool = disks.is_some_and(|l| l.iter().any(|d| d.pool_member == Ok(true)));
    let fold = if pool_folded(st, disks.map_or(0, Vec::len)) {
        " expand pool  "
    } else {
        " fold pool  "
    };
    let overview: Vec<(&str, &str)> = if pool {
        vec![
            ("r", " reset min/max  "),
            ("z", " zoom  "),
            ("d", fold),
            ("t", " theme  "),
        ]
    } else {
        vec![
            ("r", " reset min/max  "),
            ("z", " zoom  "),
            ("t", " theme  "),
        ]
    };
    let sensors: Vec<(&str, &str)> = if st.sensor_overflow || st.sensor_offset > 0 {
        vec![("↑↓", " scroll  "), ("r", " reset min/max  ")]
    } else {
        vec![("r", " reset min/max  ")]
    };
    let pairs: &[(&str, &str)] = match st.tab {
        Tab::Overview => &overview,
        Tab::Processes | Tab::Gpu if st.filtering => &[
            (" type", " to filter  "),
            ("enter", " keep  "),
            ("esc", " clear  "),
        ],
        Tab::Processes if !st.filter.is_empty() => &[
            ("↑↓", " select  "),
            ("←→", " sort  "),
            ("i", " invert  "),
            ("/", " filter  "),
            ("esc", " clear filter  "),
            ("e", " tree  "),
            ("del", " end process  "),
        ],
        Tab::Processes => &[
            ("↑↓", " select  "),
            ("←→", " sort  "),
            ("i", " invert  "),
            ("/", " filter  "),
            ("e", " tree  "),
            ("del", " end process  "),
        ],
        Tab::Gpu if !st.filter.is_empty() => &[
            ("↑↓", " select  "),
            ("←→", " sort  "),
            ("i", " invert  "),
            ("/", " filter  "),
            ("esc", " clear filter  "),
            ("enter", " runtimes  "),
        ],
        Tab::Gpu => &[
            ("↑↓", " select  "),
            ("←→", " sort  "),
            ("i", " invert  "),
            ("/", " filter  "),
            ("enter", " runtimes  "),
        ],
        Tab::Sensors => &sensors,
    };
    for (k, l) in pairs {
        groups.push([key(k), label(l)]);
    }
    if !st.filtering && matches!(st.tab, Tab::Overview | Tab::Gpu) && st.gpu_count() > 1 {
        groups.push([key("g"), label(" next gpu  ")]);
    }
    if !st.filtering {
        groups.push([key("p"), label(" pause  ")]);
        groups.push([key("?"), label(" help  ")]);
        groups.push([key("q"), label(" quit")]);
    }
    // O estado completo à direita, ou o curto quando as teclas não cabem com ele (uma falha
    // aparece sempre inteira).
    let failing = failing_sources(st);
    let status = |narrow: bool| -> Vec<Span<'static>> {
        let mut right = Vec::new();
        if let Some(f) = &st.fast {
            match &f.own {
                Ok(o) => {
                    right.extend([
                        label("wintop "),
                        metric(&o.cpu, |v| format!("{}%", num(*v, 2))),
                        Span::raw(" "),
                        metric(&o.private_ws, |v| bytes(*v)),
                    ]);
                    if !narrow || o.commit.is_err() {
                        right.extend([
                            label(" ("),
                            metric(&o.commit, |v| format!("{} commit", bytes(*v))),
                            label(")"),
                        ]);
                    }
                }
                Err(e) => right.extend([label("wintop "), missing(e)]),
            }
            if !narrow {
                right.extend([
                    bar_sep(),
                    label(&format!(
                        "collection {} ms",
                        num(f.took.as_secs_f64() * 1e3, 1)
                    )),
                ]);
            }
        }
        if !right.is_empty() {
            right.push(bar_sep());
        }
        right.push(match failing {
            0 if narrow => Span::styled(tr("all OK ").into_owned(), Style::new().fg(th().ok)),
            0 => Span::styled(
                tr("sources: all OK ").into_owned(),
                Style::new().fg(th().ok),
            ),
            1 => Span::styled(
                tr("! 1 source failing ").into_owned(),
                Style::new().fg(th().fail),
            ),
            n => Span::styled(
                tr(&format!("! {n} sources failing ")).into_owned(),
                Style::new().fg(th().fail),
            ),
        });
        right
    };
    let keys_w: usize = groups.iter().map(|g| g[0].width() + g[1].width()).sum();
    let full = status(false);
    let full_w: usize = full.iter().map(Span::width).sum();
    let right = if keys_w + full_w + 2 <= usize::from(area.width) {
        full
    } else {
        status(true)
    };
    // O lado direito (estado) tem prioridade; a ajuda usa o que sobra, com 2 colunas de folga.
    let right = Line::from(right);
    let rw = (right.width() as u16).min(area.width);
    let room = usize::from(area.width.saturating_sub(rw).saturating_sub(2));
    // Prioridade na falta de espaço: abas, sair, ajuda, pausa e depois os atalhos da aba;
    // digitando o filtro só há os atalhos dele, na ordem. A ordem na tela continua a natural.
    let n = groups.len();
    let mut priority: Vec<usize> = if st.filtering || n < 4 {
        (0..n).collect()
    } else {
        // Abas, sair, ajuda, os atalhos da aba e por último a pausa.
        [0, n - 1, n - 2]
            .into_iter()
            .chain(1..n - 3)
            .chain([n - 3])
            .collect()
    };
    priority.dedup();
    let mut keep = vec![false; n];
    let mut used = 0;
    for i in priority {
        let w = groups[i][0].width() + groups[i][1].width();
        if used + w <= room {
            used += w;
            keep[i] = true;
        }
    }
    let left: Vec<Span<'static>> = groups
        .into_iter()
        .zip(keep)
        .filter(|(_, k)| *k)
        .flat_map(|(g, _)| g)
        .collect();
    let left = Line::from(left);
    let [l, r] = Layout::horizontal([Constraint::Min(0), Constraint::Length(rw)]).areas(area);
    frame.render_widget(Paragraph::new(left), l);
    frame.render_widget(Paragraph::new(right).right_aligned(), r);
}

/// Métrica em falha (estado neutro não conta).
fn failed<T>(m: &Metric<T>) -> bool {
    matches!(m, Err(Missing::Fail(_)))
}

/// Fonte em falha: o coletor inteiro, ou qualquer métrica dela (o que aparece em vermelho na
/// tela conta no rodapé). `inner`: as métricas da fonte quando o coletor respondeu.
fn source_failed<T>(m: &Metric<T>, inner: impl FnOnce(&T) -> bool) -> bool {
    match m {
        Ok(v) => inner(v),
        Err(e) => matches!(e, Missing::Fail(_)),
    }
}

/// GPUs que a tela mostra: a selecionada (tecla `g`), ou todas na aba Sensores.
fn shown_gpus<'a, T>(st: &State, l: &'a [T]) -> impl Iterator<Item = &'a T> {
    let (skip, take) = if st.tab == Tab::Sensors {
        (0, l.len())
    } else {
        (st.gpu_index(l.len()), 1)
    };
    l.iter().skip(skip).take(take)
}

/// Fontes com alguma falha.
pub(super) fn failing_sources(st: &State) -> usize {
    let mut n = 0;
    let mut add = |b: bool| n += usize::from(b);
    if let Some(f) = &st.fast {
        add(source_failed(&f.cpu, Cpu::has_failure));
        add(source_failed(&f.cpu_sensors, CpuSensors::has_failure));
        add(source_failed(&f.memory, Memory::has_failure));
        add(source_failed(&f.disks, |l| l.iter().any(Disk::has_failure)));
        add(source_failed(&f.nics, |l| l.iter().any(Nic::has_failure)));
        // Só a GPU exibida (tecla `g`); na aba Sensores, que mostra todas, todas.
        add(source_failed(&f.gpus, |l: &Vec<Gpu>| {
            shown_gpus(st, l).any(Gpu::has_failure)
        }));
        add(source_failed(&f.own, OwnUsage::has_failure));
    }
    let mut slow_gpus_failed = false;
    if let Some(s) = &st.slow {
        slow_gpus_failed = s.gpus.is_err();
        // GPU sem NVML: o PCIe repete a causa que a coleta rápida já conta (`no_nvml`).
        let no_nvml = |i: u32| {
            st.fast
                .as_ref()
                .and_then(|f| f.gpus.as_ref().ok())
                .and_then(|l| l.iter().find(|g| g.index == i))
                .is_some_and(|g| g.no_nvml.is_some())
        };
        // Na lista lenta, a GPU exibida é a da posição da rápida (`slow_gpu`).
        add(source_failed(&s.gpus, |l| {
            // Na aba Sensores, só as GPUs que a lista rápida (a que a aba mostra) tem: sem ela,
            // nenhuma (a falha da rápida já conta).
            let shown: Vec<&crate::model::GpuSlow> = if st.tab == Tab::Sensors {
                l.iter().take(st.gpu_count()).collect()
            } else {
                st.slow_gpu(l).into_iter().collect()
            };
            shown.into_iter().any(|g| {
                failed(&g.vram) || (!no_nvml(g.index) && (failed(&g.pcie_tx) || failed(&g.pcie_rx)))
            })
        }));
        // Só dos discos exibidos (um disco removido ou trocado no mesmo número não aparece em
        // aba nenhuma; a lista lenta o tira na coleta seguinte).
        let shown = |t: &crate::model::DiskTemp| match &st.fast.as_ref().map(|f| &f.disks) {
            Some(Ok(disks)) => disks.iter().any(|d| same_disk(t, d)),
            _ => true,
        };
        add(source_failed(&s.disk_temps, |l| {
            l.iter()
                .any(|t| shown(t) && (failed(&t.celsius) || failed(&t.health)))
        }));
    }
    // Fontes das abas Processos e GPU (só quando visíveis). A lista da GPU herda a falha da
    // coleta lenta da GPU, já contada acima.
    if matches!(st.tab, Tab::Processes | Tab::Gpu) {
        add(matches!(&st.procs, Some(Err(Missing::Fail(_)))));
    }
    if st.tab == Tab::Gpu {
        match &st.gpu_procs {
            Some(Err(Missing::Fail(_))) if !slow_gpus_failed => add(true),
            Some(Ok(list)) => add(list.iter().any(|p| failed(&p.engines))),
            _ => {}
        }
    }
    // Runtimes pedidos na aba GPU (aparecem no detalhe dela).
    add(st.tab == Tab::Gpu && st.runtimes.values().any(|(_, r)| failed(r)));
    if let Some(f) = &st.fps {
        add(failed(&f.rtss));
        add(failed(&f.pipe));
        // O jogo sem valor herda a falha do RTSS (já contada): só as métricas dele contam.
        add(f.game.as_ref().is_ok() && source_failed(&f.game, Game::has_failure));
    }
    n
}

fn waiting(frame: &mut Frame<'_>, area: Rect, block: Block<'static>) {
    frame.render_widget(
        Paragraph::new(Span::styled(
            tr("waiting for the first collection").into_owned(),
            Style::new().fg(th().neutral),
        ))
        .block(block),
        area,
    );
}

/// Caixa sem dados: espera ou a falha do coletor.
fn empty_box<T>(frame: &mut Frame<'_>, area: Rect, title: &str, m: Option<&Metric<T>>) {
    match m {
        Some(Err(e)) => frame.render_widget(
            Paragraph::new(missing(e))
                .wrap(ratatui::widgets::Wrap { trim: false })
                .block(boxed(title, "")),
            area,
        ),
        _ => waiting(frame, area, boxed(title, "")),
    }
}

/// some e a continuação começa com `indent` espaços. `first` é o recuo da primeira linha.
pub(super) fn wrap(
    spans: Vec<Span<'static>>,
    width: u16,
    first: u16,
    indent: u16,
) -> Vec<Line<'static>> {
    let starts_group = |sp: &Span<'_>| sp.content.starts_with(char::is_whitespace);
    let mut groups: std::collections::VecDeque<Vec<Span<'static>>> = Default::default();
    for sp in spans {
        if groups.is_empty() || starts_group(&sp) {
            groups.push_back(Vec::new());
        }
        if let Some(g) = groups.back_mut() {
            g.push(sp);
        }
    }
    // Sem o espaço (e o "│") do começo do grupo, para abrir uma linha.
    let trimmed = |g: &[Span<'static>]| -> Vec<Span<'static>> {
        let mut out = Vec::with_capacity(g.len());
        let mut lead = true;
        for sp in g {
            if lead {
                let t = sp.content.trim_start().trim_start_matches('│').trim_start();
                if t.is_empty() {
                    continue;
                }
                lead = false;
                out.push(Span::styled(t.to_string(), sp.style));
            } else {
                out.push(sp.clone());
            }
        }
        out
    };
    // O grupo em palavras (cada palavra vira um grupo); `None` se já é uma palavra só.
    let words = |g: &[Span<'static>]| -> Option<Vec<Vec<Span<'static>>>> {
        let mut out: Vec<Vec<Span<'static>>> = Vec::new();
        // Espaço visto (inclusive no fim de um span): a próxima palavra abre um grupo com ele.
        let mut space = false;
        for sp in g {
            for (i, w) in sp.content.split(' ').enumerate() {
                if i > 0 {
                    space = true;
                }
                if w.is_empty() {
                    continue;
                }
                let new_group = std::mem::take(&mut space);
                let t = if new_group {
                    format!(" {w}")
                } else {
                    w.to_string()
                };
                let part = Span::styled(t, sp.style);
                if out.is_empty() || new_group {
                    out.push(vec![part]);
                } else if let Some(last) = out.last_mut() {
                    last.push(part);
                }
            }
        }
        (out.len() > 1).then_some(out)
    };
    let width = usize::from(width);
    let pad = |n: u16| Span::raw(" ".repeat(usize::from(n)));
    let mut lines = Vec::new();
    let mut cur = vec![pad(first)];
    let mut used = usize::from(first);
    let mut empty = true;
    while let Some(g) = groups.pop_front() {
        let g = if empty { trimmed(&g) } else { g };
        if g.is_empty() {
            continue;
        }
        let w: usize = g.iter().map(Span::width).sum();
        if used + w <= width {
            used += w;
            empty = false;
            cur.extend(g);
            continue;
        }
        // Não cabe nem a partir do recuo de uma linha nova: quebra por palavra.
        let start = if lines.is_empty() && empty {
            first
        } else {
            indent
        };
        let tw: usize = trimmed(&g).iter().map(Span::width).sum();
        if usize::from(start) + tw > width
            && let Some(ws) = words(&g)
        {
            for w in ws.into_iter().rev() {
                groups.push_front(w);
            }
            continue;
        }
        if empty {
            // Linha vazia e uma palavra só maior que a largura: entra cortada.
            used += w;
            empty = false;
            cur.extend(g);
            continue;
        }
        lines.push(Line::from(std::mem::take(&mut cur)));
        let g = trimmed(&g);
        used = usize::from(indent) + g.iter().map(Span::width).sum::<usize>();
        cur.push(pad(indent));
        cur.extend(g);
    }
    if !empty {
        lines.push(Line::from(cur));
    }
    lines
}

/// Largura do texto ao lado de uma barra: o valor inteiro antes da barra; sem valor, usa toda a
/// largura depois do rótulo para a causa.
fn text_room(t: &Span<'_>, missing: bool, width: u16, label_w: u16) -> u16 {
    let w = t.width() as u16;
    if missing {
        w.min(width.saturating_sub(label_w))
    } else {
        // O valor inteiro antes da barra: a barra encolhe (ex.: "(69%)" não perde o parêntese).
        w.min(width.saturating_sub(label_w + 4))
    }
}

/// Linha "rótulo ⣿⣿ usado/total" dentro de `area`, na linha `row`.
fn bar_line(frame: &mut Frame<'_>, area: Rect, row: u16, name: &str, m: &Metric<(u64, u64)>) {
    if row >= area.height {
        return;
    }
    let y = area.y + row;
    let t = metric(m, |(u, t)| {
        format!(
            " {} / {} ({}%)",
            bytes(*u),
            bytes(*t),
            num(*u as f64 / (*t).max(1) as f64 * 100.0, 0)
        )
    });
    let label_w = (name.len() as u16 + 1).min(area.width);
    let text_w = text_room(&t, m.is_err(), area.width, label_w);
    line(frame, vec![label(name)], area.x, y, label_w, area);
    let bar_w = area.width.saturating_sub(label_w + text_w);
    if let Ok((u, t)) = m {
        let v = if *t > 0 { *u as f64 / *t as f64 } else { 0.0 };
        frame.render_widget(
            Meter { value: v },
            Rect::new(area.x + label_w, y, bar_w, 1).intersection(area),
        );
    }
    put(
        frame,
        Paragraph::new(t),
        Rect::new(area.x + label_w + bar_w, y, text_w, 1),
        area,
    );
}
