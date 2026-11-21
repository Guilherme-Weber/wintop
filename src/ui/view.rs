//! Aba Geral (v2, a partir do mockup da revisão visual, aprovado pelo usuário): barra de abas,
//! CPU (gráfico total + grade de gráficos por lógico) e GPU (engine mais usada, VRAM por
//! categoria) no topo; memória e rede embaixo à esquerda; discos e jogo (FPS + frametime)
//! embaixo à direita; rodapé com atalhos e o consumo do próprio wintop.
//!
//! Direção do usuário: uso em gráfico; RAM e VRAM em barra; temperaturas só como número;
//! bordas neutras estilo btop; mínimo/máximo da sessão (tecla `r` zera). Desenho puro a partir
//! do estado (snapshots do `insta`).

use std::collections::{HashMap, VecDeque};

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};

use super::widgets::{DIM, Graph, GraphColor, Meter, Segmented};
use crate::fmt::{bits, bytes, num};
use crate::model::{Disk, Fast, Fps, Gpu, Metric, Missing, Nic, Slow, VramBreakdown, VramKind};
use crate::win::printable;

/// Amostras guardadas por série (cobre ~500 colunas de braille).
const HISTORY: usize = 1000;
/// Escala mínima da rede (100 Kbps): ruído de poucos bytes não parece tráfego.
const NET_FLOOR: f64 = 100_000.0;
/// Teto do frametime (como o Afterburner) e o limite de alerta (30 FPS).
const FT_CEIL_MS: f64 = 50.0;
const FT_ALERT_MS: f64 = 33.3;
/// Degraus do teto do gráfico de FPS (sobe para o próximo quando o máximo passa).
const FPS_STEPS: [f64; 8] = [60.0, 120.0, 165.0, 240.0, 360.0, 500.0, 750.0, 1000.0];

const BORDER: Color = Color::Rgb(75, 82, 99);
const TITLE: Color = Color::Rgb(220, 224, 232);
const TEXT: Color = Color::Rgb(200, 204, 212);
const LABEL: Color = Color::Rgb(127, 133, 148);
const NEUTRAL: Color = Color::Rgb(108, 112, 128);
const FAIL: Color = Color::Rgb(235, 80, 80);
const NET_SEND: Color = Color::Rgb(200, 120, 200);
const NET_RECV: Color = Color::Rgb(92, 184, 228);
const FPS_COLOR: Color = Color::Rgb(80, 200, 120);
const FT_COLOR: Color = Color::Rgb(224, 168, 75);
const ENCODE: Color = Color::Rgb(86, 194, 214);
const DECODE: Color = Color::Rgb(176, 132, 224);

/// Aba visível.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    Overview,
    Processes,
    Gpu,
    Sensors,
}

/// Série com tamanho limitado.
#[derive(Default)]
pub struct Series(VecDeque<f64>);

impl Series {
    pub fn push(&mut self, v: f64) {
        if self.0.len() == HISTORY {
            self.0.pop_front();
        }
        self.0.push_back(if v.is_finite() { v } else { 0.0 });
    }

    fn values(&mut self) -> &[f64] {
        self.0.make_contiguous()
    }

    /// As últimas `n` amostras divididas por `ceil`.
    fn over(&mut self, n: usize, ceil: f64) -> Vec<f64> {
        let v = self.values();
        let tail = &v[v.len().saturating_sub(n)..];
        tail.iter()
            .map(|x| if ceil > 0.0 { x / ceil } else { 0.0 })
            .collect()
    }

    /// Maior valor entre as últimas `n` amostras.
    fn max_of(&mut self, n: usize) -> f64 {
        let v = self.values();
        v[v.len().saturating_sub(n)..]
            .iter()
            .copied()
            .fold(0.0, f64::max)
    }
}

/// Mínimo e máximo da sessão (zerados com `r`).
#[derive(Clone, Copy, Default)]
pub struct MinMax(Option<(f64, f64)>);

impl MinMax {
    fn update(&mut self, v: f64) {
        if !v.is_finite() {
            return;
        }
        self.0 = Some(match self.0 {
            None => (v, v),
            Some((lo, hi)) => (lo.min(v), hi.max(v)),
        });
    }

    fn get(self) -> Option<(f64, f64)> {
        self.0
    }
}

/// Extremos da sessão de cada série exibida.
#[derive(Default)]
struct Session {
    cpu: MinMax,
    tctl: MinMax,
    cpu_power: MinMax,
    gpu: MinMax,
    gpu_temp: MinMax,
    gpu_power: MinMax,
    vram: MinMax,
    gfx_clock: MinMax,
    ram: MinMax,
    commit: MinMax,
    send: HashMap<String, MinMax>,
    recv: HashMap<String, MinMax>,
    fps: MinMax,
    ft: MinMax,
}

/// Estado da interface: os últimos dados, o histórico dos gráficos e os extremos da sessão.
#[derive(Default)]
pub struct State {
    pub fast: Option<Fast>,
    pub slow: Option<Slow>,
    pub fps: Option<Fps>,
    pub interval_ms: u64,
    pub tab: Tab,
    /// Membros do Storage Space recolhidos: `None` = automático (recolhe se não couberem).
    pub fold_pool: Option<bool>,
    /// Relógio da barra de abas (vem de fora: o desenho é puro).
    pub clock: String,
    cpu: Series,
    cores: Vec<Series>,
    gpu: Series,
    disks: HashMap<u32, Series>,
    send: HashMap<String, Series>,
    recv: HashMap<String, Series>,
    game_fps: Series,
    game_ft: Series,
    game_pid: Option<u32>,
    /// FPS mais recente e o pior frametime (ms) desde o último tick da UI.
    pending_fps: Option<f64>,
    pending_ft: Option<f64>,
    session: Session,
}

impl State {
    pub fn new(interval_ms: u64) -> Self {
        Self {
            interval_ms,
            ..Default::default()
        }
    }

    /// Zera os extremos da sessão (tecla `r`).
    pub fn reset_session(&mut self) {
        self.session = Session::default();
    }

    /// Nova coleta da thread rápida: guarda, alimenta os históricos (inclusive os do jogo, no
    /// mesmo ritmo) e os extremos da sessão.
    pub fn push_fast(&mut self, f: Fast) {
        let usage = |m: &Metric<f64>| m.as_ref().map_or(0.0, |v| v / 100.0);
        if let Ok(c) = &f.cpu {
            self.cpu.push(usage(&c.usage));
            if let Ok(v) = &c.usage {
                self.session.cpu.update(*v);
            }
            if let Ok(per) = &c.per_logical {
                self.cores.resize_with(per.len(), Series::default);
                for (s, v) in self.cores.iter_mut().zip(per) {
                    s.push(v / 100.0);
                }
            }
        }
        if let Ok(s) = &f.cpu_sensors {
            if let Ok(t) = &s.tctl {
                self.session.tctl.update(*t);
            }
            if let Ok(w) = &s.package_power {
                self.session.cpu_power.update(*w);
            }
        }
        if let Ok(list) = &f.gpus
            && let Some(g) = list.first()
        {
            let (_, v) = busiest_engine(g);
            self.gpu.push(f64::from(v) / 100.0);
            self.session.gpu.update(f64::from(v));
            if let Ok(t) = &g.temp {
                self.session.gpu_temp.update(f64::from(*t));
            }
            if let Ok((p, _)) = &g.power {
                self.session.gpu_power.update(*p);
            }
            if let Ok((u, _)) = &g.dedicated {
                self.session.vram.update(*u as f64);
            }
            if let Ok((c, _)) = &g.clock_graphics {
                self.session.gfx_clock.update(f64::from(*c));
            }
        }
        if let Ok(m) = &f.memory {
            if let Ok(u) = &m.used {
                self.session.ram.update(*u as f64);
            }
            if let Ok((c, _)) = &m.commit {
                self.session.commit.update(*c as f64);
            }
        }
        if let Ok(list) = &f.disks {
            self.disks
                .retain(|n, _| list.iter().any(|d| d.number == *n));
            for d in list {
                self.disks
                    .entry(d.number)
                    .or_default()
                    .push(usage(&d.active));
            }
        }
        if let Ok(list) = &f.nics {
            self.send.retain(|a, _| list.iter().any(|n| &n.alias == a));
            self.recv.retain(|a, _| list.iter().any(|n| &n.alias == a));
            for n in list {
                let v = |m: &Metric<f64>| m.as_ref().copied().unwrap_or(0.0);
                self.send
                    .entry(n.alias.clone())
                    .or_default()
                    .push(v(&n.send));
                self.recv
                    .entry(n.alias.clone())
                    .or_default()
                    .push(v(&n.recv));
                if let Ok(s) = &n.send {
                    self.session
                        .send
                        .entry(n.alias.clone())
                        .or_default()
                        .update(*s);
                }
                if let Ok(r) = &n.recv {
                    self.session
                        .recv
                        .entry(n.alias.clone())
                        .or_default()
                        .update(*r);
                }
            }
        }
        // Jogo no ritmo da UI: 1 ponto por tick (FPS mais recente; pior frametime do tick).
        if let Some(v) = self.pending_fps.take() {
            self.game_fps.push(v);
            self.session.fps.update(v);
        }
        if let Some(v) = self.pending_ft.take() {
            self.game_ft.push(v);
            self.session.ft.update(v);
        }
        self.fast = Some(f);
    }

    /// Novo estado do RTSS (a cada 250 ms com jogo): o número de FPS atualiza na hora; os
    /// gráficos esperam o tick da UI.
    pub fn push_fps(&mut self, s: Fps) {
        if let Ok(g) = &s.game {
            if self.game_pid != Some(g.pid) {
                self.game_pid = Some(g.pid);
                self.game_fps = Series::default();
                self.game_ft = Series::default();
                self.session.fps = MinMax::default();
                self.session.ft = MinMax::default();
            }
            if let Ok(v) = &g.fps {
                self.pending_fps = Some(*v);
            }
            if let Ok((_, _, hi)) = &g.frametime {
                self.pending_ft = Some(self.pending_ft.map_or(*hi, |p| p.max(*hi)));
            }
        }
        self.fps = Some(s);
    }
}

// --- Texto ---

/// Valor, estado neutro (cinza) ou falha (vermelho, com a causa); sem controles.
fn metric<T>(m: &Metric<T>, f: impl FnOnce(&T) -> String) -> Span<'static> {
    match m {
        Ok(v) => Span::styled(printable(&f(v)), Style::new().fg(TEXT)),
        Err(Missing::Neutral(s)) => Span::styled(printable(s), Style::new().fg(NEUTRAL)),
        Err(Missing::Fail(s)) => Span::styled(format!("! {}", printable(s)), Style::new().fg(FAIL)),
    }
}

fn missing(e: &Missing) -> Span<'static> {
    metric::<()>(&Err(e.clone()), |()| String::new())
}

fn label(s: &str) -> Span<'static> {
    Span::styled(s.to_string(), Style::new().fg(LABEL))
}

fn text(s: impl Into<String>) -> Span<'static> {
    Span::styled(s.into(), Style::new().fg(TEXT))
}

fn sep() -> Span<'static> {
    Span::raw("  ")
}

fn bar_sep() -> Span<'static> {
    Span::styled("  │  ", Style::new().fg(DIM))
}

/// "min X max Y" de um extremo da sessão.
fn minmax(m: MinMax, f: impl Fn(f64) -> String) -> Span<'static> {
    match m.get() {
        Some((lo, hi)) => label(&format!("min {} max {}", f(lo), f(hi))),
        None => label(""),
    }
}

/// Caixa com borda neutra, título à esquerda e contexto à direita, na própria borda.
fn boxed(title: &str, right: &str) -> Block<'static> {
    let mut b = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(BORDER))
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                printable(title),
                Style::new().fg(TITLE).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
        ]));
    if !right.is_empty() {
        b = b.title(
            Line::from(Span::styled(format!(" {right} "), Style::new().fg(LABEL))).right_aligned(),
        );
    }
    b
}

/// Desenha um parágrafo recortado a `bound`.
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

fn temp(m: &Metric<f64>) -> Span<'static> {
    metric(m, |v| format!("{} °C", num(*v, 1)))
}

// --- Layout ---

pub fn draw(frame: &mut Frame<'_>, st: &mut State) {
    let [tabs, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(8),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    draw_tabs(frame, tabs, st);
    draw_footer(frame, footer, st);
    if st.tab != Tab::Overview {
        let name = match st.tab {
            Tab::Processes => "processes",
            Tab::Gpu => "gpu",
            _ => "sensors",
        };
        frame.render_widget(
            Paragraph::new(label(
                "this tab arrives in stage 6 (press 1 for the overview)",
            ))
            .block(boxed(name, "")),
            body,
        );
        return;
    }
    let [top, bottom] =
        Layout::vertical([Constraint::Percentage(48), Constraint::Percentage(52)]).areas(body);
    let [cpu, gpu] =
        Layout::horizontal([Constraint::Percentage(59), Constraint::Percentage(41)]).areas(top);
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(46), Constraint::Percentage(54)]).areas(bottom);
    let [mem, net] = Layout::vertical([Constraint::Length(10), Constraint::Min(4)]).areas(left);
    // Discos: cabeçalho + uma linha por disco visível; no máximo metade da coluna.
    let disk_rows = disk_order(st).len() as u16;
    let disks_h = (disk_rows + 3).min(right.height / 2).max(4);
    let [disks, game] =
        Layout::vertical([Constraint::Length(disks_h), Constraint::Min(4)]).areas(right);
    draw_cpu(frame, cpu, st);
    draw_gpu(frame, gpu, st);
    draw_memory(frame, mem, st);
    draw_net(frame, net, st);
    draw_disks(frame, disks, st);
    draw_game(frame, game, st);
}

fn draw_tabs(frame: &mut Frame<'_>, area: Rect, st: &State) {
    let tab = |n: &str, name: &str, t: Tab| -> Span<'static> {
        if st.tab == t {
            Span::styled(
                format!(" [{n} {name}] "),
                Style::new().fg(TITLE).add_modifier(Modifier::BOLD),
            )
        } else {
            label(&format!("  {n} {name}  "))
        }
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            tab("1", "Overview", Tab::Overview),
            tab("2", "Processes", Tab::Processes),
            tab("3", "GPU", Tab::Gpu),
            tab("4", "Sensors", Tab::Sensors),
        ])),
        area,
    );
    let up = st
        .fast
        .as_ref()
        .and_then(|f| f.cpu.as_ref().ok())
        .map(|c| c.uptime.as_secs());
    let mut right = vec![label(&format!("interval {} ms", st.interval_ms))];
    if let Some(s) = up {
        right.extend([
            bar_sep(),
            label(&format!(
                "up {}d {:02}:{:02}:{:02}",
                s / 86400,
                s / 3600 % 24,
                s / 60 % 60,
                s % 60
            )),
        ]);
    }
    if !st.clock.is_empty() {
        right.extend([bar_sep(), text(st.clock.clone())]);
    }
    frame.render_widget(Paragraph::new(Line::from(right)).right_aligned(), area);
}

fn draw_footer(frame: &mut Frame<'_>, area: Rect, st: &State) {
    let key = |k: &str| {
        Span::styled(
            k.to_string(),
            Style::new().fg(TITLE).add_modifier(Modifier::BOLD),
        )
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            key(" 1-4"),
            label(" tabs  "),
            key("+/-"),
            label(&format!(" interval {} ms  ", st.interval_ms)),
            key("r"),
            label(" reset min/max  "),
            key("d"),
            label(" fold pool  "),
            key("q"),
            label(" quit"),
        ])),
        area,
    );
    let mut right = Vec::new();
    if let Some(f) = &st.fast {
        if let Ok(o) = &f.own {
            right.extend([
                label("wintop "),
                metric(&o.cpu, |v| format!("{}%", num(*v, 2))),
                Span::raw(" "),
                metric(&o.private_ws, |v| bytes(*v)),
                label(" ("),
                metric(&o.commit, |v| format!("{} commit", bytes(*v))),
                label(")"),
            ]);
        }
        right.extend([
            bar_sep(),
            label(&format!(
                "collection {} ms",
                num(f.took.as_secs_f64() * 1e3, 1)
            )),
        ]);
    }
    let failing = failing_sources(st);
    right.extend([
        bar_sep(),
        if failing == 0 {
            label("sources: all OK ")
        } else {
            Span::styled(
                format!("! {failing} sources failing "),
                Style::new().fg(FAIL),
            )
        },
    ]);
    frame.render_widget(Paragraph::new(Line::from(right)).right_aligned(), area);
}

/// Fontes inteiras em falha (coletor sem nenhum dado).
fn failing_sources(st: &State) -> usize {
    let fail = |e: &Missing| matches!(e, Missing::Fail(_));
    let mut n = 0;
    if let Some(f) = &st.fast {
        n += usize::from(f.cpu.as_ref().is_err_and(fail));
        n += usize::from(f.cpu_sensors.as_ref().is_err_and(fail));
        n += usize::from(f.memory.as_ref().is_err_and(fail));
        n += usize::from(f.disks.as_ref().is_err_and(fail));
        n += usize::from(f.nics.as_ref().is_err_and(fail));
        n += usize::from(f.gpus.as_ref().is_err_and(fail));
    }
    if let Some(s) = &st.slow {
        n += usize::from(s.gpus.as_ref().is_err_and(fail));
        n += usize::from(s.disk_temps.as_ref().is_err_and(fail));
    }
    if let Some(f) = &st.fps {
        n += usize::from(f.rtss.as_ref().is_err_and(fail));
    }
    n
}

fn waiting(frame: &mut Frame<'_>, area: Rect, block: Block<'static>) {
    frame.render_widget(
        Paragraph::new(Span::styled(
            "waiting for the first collection",
            Style::new().fg(NEUTRAL),
        ))
        .block(block),
        area,
    );
}

/// Caixa sem dados: espera ou a falha do coletor.
fn empty_box<T>(frame: &mut Frame<'_>, area: Rect, title: &str, m: Option<&Metric<T>>) {
    match m {
        Some(Err(e)) => {
            frame.render_widget(Paragraph::new(missing(e)).block(boxed(title, "")), area)
        }
        _ => waiting(frame, area, boxed(title, "")),
    }
}

// --- CPU ---

/// Colunas da grade por lógico, pelo número real de threads.
fn grid_cols(n: usize) -> usize {
    match n {
        0..=4 => 2,
        5..=16 => 4,
        17..=24 => 6,
        _ => 8,
    }
}

fn draw_cpu(frame: &mut Frame<'_>, area: Rect, st: &mut State) {
    let Some(Ok(c)) = st.fast.as_ref().map(|f| &f.cpu) else {
        empty_box(frame, area, "cpu", st.fast.as_ref().map(|f| &f.cpu));
        return;
    };
    let name = c
        .name
        .as_ref()
        .map_or_else(|e| format!("cpu · {e}"), |n| format!("cpu · {n}"));
    let per_len = c.per_logical.as_ref().map_or(0, Vec::len);
    let block = boxed(&name, &format!("total 60s · {per_len} logical"));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [graphs, info] = Layout::vertical([Constraint::Min(3), Constraint::Length(2)]).areas(inner);
    // Total à esquerda (30 colunas = 60 s a 1 s), grade por lógico no resto.
    let total_w = (graphs.width / 4).clamp(16, 34);
    let [total, grid] =
        Layout::horizontal([Constraint::Length(total_w), Constraint::Min(8)]).areas(graphs);

    // Gráfico total, com o valor e os extremos na 1ª linha.
    let head = vec![
        metric(&c.usage, |v| format!("{}%", num(*v, 0))).add_modifier(Modifier::BOLD),
        Span::raw(" "),
        label("kern "),
        metric(&c.kernel, |v| format!("{}%", num(*v, 0))),
    ];
    line(frame, head, total.x, total.y, total.width, total);
    let mm = minmax(st.session.cpu, |v| num(v, 0));
    line(frame, vec![mm], total.x, total.y + 1, total.width, total);
    let g = Rect::new(
        total.x,
        total.y + 2,
        total.width.saturating_sub(1),
        total.height.saturating_sub(2),
    );
    frame.render_widget(
        Graph::new(st.cpu.values(), GraphColor::Height),
        g.intersection(total),
    );

    match &c.per_logical {
        Ok(per) => draw_core_grid(frame, grid, &mut st.cores, per),
        Err(e) => line(
            frame,
            vec![label("per core "), missing(e)],
            grid.x,
            grid.y,
            grid.width,
            grid,
        ),
    }

    // Linhas de sensores e topologia.
    let sensors = st.fast.as_ref().map(|f| &f.cpu_sensors);
    let mut l1 = vec![
        label("speed "),
        metric(&c.freq_mhz, |v| format!("{} GHz", num(v / 1000.0, 2))),
        metric(&c.base_mhz, |m| {
            format!(" (base {})", num(f64::from(*m) / 1000.0, 2))
        }),
    ];
    match sensors {
        Some(Ok(s)) => {
            l1.extend([sep(), label("Tctl "), temp(&s.tctl)]);
            if let Some((_, hi)) = st.session.tctl.get() {
                l1.push(label(&format!(" (max {})", num(hi, 1))));
            }
            match &s.ccds {
                Ok(list) => {
                    for (n, t) in list {
                        l1.extend([sep(), label(&format!("CCD{n} ")), temp(&Ok(*t))]);
                    }
                }
                Err(e) => l1.extend([sep(), label("CCD "), missing(e)]),
            }
            l1.extend([
                sep(),
                label("power "),
                metric(&s.package_power, |v| format!("{} W", num(*v, 1))),
            ]);
            if let Some((_, hi)) = st.session.cpu_power.get() {
                l1.push(label(&format!(" (max {})", num(hi, 1))));
            }
        }
        Some(Err(e)) => l1.extend([sep(), label("sensors "), missing(e)]),
        None => {}
    }
    let mut l2 = vec![metric(&c.topology, |t| {
        format!(
            "{}S · {}C/{}T  L1 {}  L2 {}  L3 {}",
            t.sockets,
            t.cores,
            t.logical,
            bytes(t.l1),
            bytes(t.l2),
            bytes(t.l3)
        )
    })];
    l2.extend([
        sep(),
        label("virt "),
        metric(&c.virtualization, |v| {
            if *v {
                "enabled".into()
            } else {
                "disabled".into()
            }
        }),
        bar_sep(),
        metric(&c.counts, |n| {
            format!(
                "{} proc  {} thr  {} handles",
                n.processes, n.threads, n.handles
            )
        }),
    ]);
    put(
        frame,
        Paragraph::new(vec![Line::from(l1), Line::from(l2)]),
        info,
        info,
    );
}

/// Grade de gráficos por lógico (4×4 com 16 threads), sempre com todos: se a célula ficar
/// pequena demais para gráfico, vira uma lista compacta de percentuais.
fn draw_core_grid(frame: &mut Frame<'_>, area: Rect, cores: &mut [Series], per: &[f64]) {
    let n = per.len();
    if n == 0 || area.width == 0 || area.height == 0 {
        return;
    }
    let cols = grid_cols(n);
    let rows = n.div_ceil(cols);
    let cw = area.width / cols as u16;
    let ch = area.height / rows as u16;
    if cw < 8 || ch < 2 {
        // Lista compacta: todos os lógicos, quebrando linha.
        let spans: Vec<Span<'static>> = per
            .iter()
            .enumerate()
            .flat_map(|(i, v)| {
                [
                    label(&format!("C{i} ")),
                    text(format!("{:>3}%  ", num(*v, 0))),
                ]
            })
            .collect();
        frame.render_widget(
            Paragraph::new(Line::from(spans)).wrap(ratatui::widgets::Wrap { trim: true }),
            area,
        );
        return;
    }
    for (i, v) in per.iter().enumerate() {
        let (c, r) = ((i % cols) as u16, (i / cols) as u16);
        let cell = Rect::new(area.x + c * cw, area.y + r * ch, cw.saturating_sub(1), ch)
            .intersection(area);
        line(
            frame,
            vec![
                label(&format!("C{i:<2} ")),
                text(format!("{:>3}%", num(*v, 0))),
            ],
            cell.x,
            cell.y,
            cell.width,
            cell,
        );
        if let Some(s) = cores.get_mut(i) {
            let g = Rect::new(
                cell.x,
                cell.y + 1,
                cell.width,
                cell.height.saturating_sub(1),
            );
            frame.render_widget(
                Graph::new(s.values(), GraphColor::Height),
                g.intersection(cell),
            );
        }
    }
}

// --- GPU ---

/// Engine mais usada (NVML): 3D/compute, encoder ou decoder, como o Task Manager mostra o
/// máximo dos engines. O Copy não existe no NVML.
fn busiest_engine(g: &Gpu) -> (&'static str, u32) {
    let get = |m: &Metric<u32>| m.as_ref().copied().unwrap_or(0);
    let mut best = ("3D", get(&g.usage));
    for (name, m) in [("encode", &g.encoder), ("decode", &g.decoder)] {
        if get(m) > best.1 {
            best = (name, get(m));
        }
    }
    best
}

fn engine_color(name: &str) -> Color {
    match name {
        "encode" => ENCODE,
        "decode" => DECODE,
        _ => TEXT,
    }
}

fn draw_gpu(frame: &mut Frame<'_>, area: Rect, st: &mut State) {
    let gpus = st.fast.as_ref().map(|f| &f.gpus);
    let Some(Ok(list)) = gpus else {
        empty_box(frame, area, "gpu", gpus);
        return;
    };
    let Some(g) = list.first().cloned() else {
        frame.render_widget(
            Paragraph::new(missing(&Missing::Fail("NVML reported no GPU".into())))
                .block(boxed("gpu", "")),
            area,
        );
        return;
    };
    let (engine, engine_v) = busiest_engine(&g);
    let name = g
        .name
        .as_ref()
        .map_or_else(|e| format!("gpu · {e}"), |n| format!("gpu · {n}"));
    let block = boxed(&name, &format!("active: {engine}"));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [top, bottom] = Layout::vertical([Constraint::Min(4), Constraint::Length(8)]).areas(inner);
    let [graph, panel] =
        Layout::horizontal([Constraint::Min(10), Constraint::Length(27)]).areas(top);

    // Gráfico da engine dominante, com o nome dela no canto.
    let head = if g.usage.is_ok() {
        vec![
            text(format!("{engine_v}% ")).add_modifier(Modifier::BOLD),
            Span::styled(engine.to_string(), Style::new().fg(engine_color(engine))),
            sep(),
            minmax(st.session.gpu, |v| format!("{}%", num(v, 0))),
        ]
    } else {
        vec![metric(&g.usage, |v| format!("{v}%"))]
    };
    line(frame, head, graph.x, graph.y, graph.width, graph);
    let gr = Rect::new(
        graph.x,
        graph.y + 1,
        graph.width.saturating_sub(1),
        graph.height.saturating_sub(1),
    );
    frame.render_widget(
        Graph::new(st.gpu.values(), GraphColor::Height),
        gr.intersection(graph),
    );

    // Painel das engines e dos extremos da sessão.
    let pct = |m: &Metric<u32>| m.as_ref().copied().unwrap_or(0);
    let eng = |name: &str, m: &Metric<u32>| -> Vec<Span<'static>> {
        let style = if name == engine {
            Style::new().fg(TITLE).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(LABEL)
        };
        vec![
            Span::styled(format!("{name:<8}"), style),
            metric(m, |v| format!("{v:>3}%")),
        ]
    };
    let mm = |m: MinMax, f: &dyn Fn(f64) -> String| -> Span<'static> {
        m.get().map_or_else(
            || label("-"),
            |(lo, hi)| text(format!("{} / {}", f(lo), f(hi))),
        )
    };
    let mut rows: Vec<(Vec<Span<'static>>, Option<f64>)> = vec![
        (eng("3D", &g.usage), Some(f64::from(pct(&g.usage)))),
        (eng("encode", &g.encoder), Some(f64::from(pct(&g.encoder)))),
        (eng("decode", &g.decoder), Some(f64::from(pct(&g.decoder)))),
        (
            eng("mem act", &g.mem_activity),
            Some(f64::from(pct(&g.mem_activity))),
        ),
        (vec![label("session min / max")], None),
    ];
    rows.push((
        vec![
            label("util  "),
            mm(st.session.gpu, &|v| format!("{}%", num(v, 0))),
        ],
        None,
    ));
    rows.push((
        vec![
            label("temp  "),
            mm(st.session.gpu_temp, &|v| format!("{} °C", num(v, 0))),
        ],
        None,
    ));
    rows.push((
        vec![
            label("power "),
            mm(st.session.gpu_power, &|v| format!("{} W", num(v, 0))),
        ],
        None,
    ));
    rows.push((
        vec![
            label("VRAM  "),
            mm(st.session.vram, &|v| short_bytes(v as u64)),
        ],
        None,
    ));
    rows.push((
        vec![
            label("gfx   "),
            mm(st.session.gfx_clock, &|v| format!("{} MHz", num(v, 0))),
        ],
        None,
    ));
    for (i, (spans, meter)) in rows.into_iter().enumerate() {
        let y = panel.y + i as u16;
        if y >= panel.y + panel.height {
            break;
        }
        // Linhas com medidor: rótulo + valor em 13 colunas; as de extremos usam o painel todo.
        let w = if meter.is_some() {
            13
        } else {
            panel.width.saturating_sub(1)
        };
        line(frame, spans, panel.x + 1, y, w, panel);
        if let Some(v) = meter {
            frame.render_widget(
                Meter { value: v / 100.0 },
                Rect::new(panel.x + 15, y, panel.width.saturating_sub(15), 1).intersection(panel),
            );
        } else if i > 4 {
            // As linhas de extremos usam a largura toda (o texto já veio inteiro no `spans`).
        }
    }

    // VRAM por categoria, compartilhada, física e o resto das leituras.
    let slow = st
        .slow
        .as_ref()
        .and_then(|s| s.gpus.as_ref().ok())
        .and_then(|l| l.iter().find(|x| x.index == g.index))
        .cloned();
    let slow_err = match st.slow.as_ref().map(|s| &s.gpus) {
        Some(Err(e)) => Some(e.clone()),
        _ => None,
    };
    let game_pid = st
        .fps
        .as_ref()
        .and_then(|f| f.game.as_ref().ok())
        .map(|g| g.pid);
    let b = bottom;
    match slow.as_ref().map(|s| &s.vram) {
        Some(Ok(v)) => vram_bar(
            frame,
            Rect::new(b.x, b.y, b.width, 2).intersection(b),
            v,
            game_pid,
            &g.dedicated,
        ),
        _ => bar_line(frame, b, 0, "VRAM  ", &g.dedicated),
    }
    bar_line(frame, b, 2, "shared", &g.shared);
    let clk = |m: &Metric<(u32, u32)>| metric(m, |(c, mx)| format!("{c}/{mx}"));
    let rows: Vec<Vec<Span<'static>>> = vec![
        vec![
            label("phys  "),
            metric(&g.physical, |[u, f, r, t]| {
                format!(
                    "{} used · {} free · {} reserved · {} total (NVML)",
                    bytes(*u),
                    bytes(*f),
                    bytes(*r),
                    bytes(*t)
                )
            }),
        ],
        vec![
            label("clk   gfx "),
            clk(&g.clock_graphics),
            label("  SM "),
            clk(&g.clock_sm),
            label("  mem "),
            clk(&g.clock_mem),
            label("  video "),
            clk(&g.clock_video),
            label(" MHz"),
        ],
        vec![
            label("temp  "),
            metric(&g.temp, |v| format!("{v} °C")),
            metric(&g.temp_slowdown, |v| format!(" (slowdown {v} °C)")),
            sep(),
            label("power "),
            metric(&g.power, |(p, l)| {
                format!("{}/{} W", num(*p, 0), num(*l, 0))
            }),
            sep(),
            metric(&g.pstate, |v| format!("P{v}")),
            sep(),
            label("throttle "),
            metric(&g.throttle, |v| {
                if v.is_empty() {
                    "none".into()
                } else {
                    v.join(", ")
                }
            }),
        ],
        {
            let mut v = vec![label("fans  ")];
            match &g.fans {
                Ok(list) => {
                    for (i, f) in list.iter().enumerate() {
                        v.extend([
                            label(&format!("fan{i} ")),
                            metric(&f.percent, |p| format!("{p}% ")),
                            metric(&f.rpm, |r| format!("{r} rpm")),
                            sep(),
                        ]);
                    }
                }
                Err(e) => v.push(missing(e)),
            }
            v
        },
        {
            let mut v = vec![
                label("PCIe  "),
                metric(&g.pcie_link, |(gen_, w)| format!("Gen{gen_} x{w}")),
            ];
            if let Some(s) = &slow {
                v.extend([
                    label("  tx "),
                    metric(&s.pcie_tx, |x| format!("{}/s", bytes(*x))),
                    label("  rx "),
                    metric(&s.pcie_rx, |x| format!("{}/s", bytes(*x))),
                ]);
            } else if let Some(e) = &slow_err {
                v.extend([label("  tx/rx "), missing(e)]);
            }
            v.extend([
                bar_sep(),
                label("drv "),
                metric(&g.driver, Clone::clone),
                label("  VBIOS "),
                metric(&g.vbios, Clone::clone),
            ]);
            v
        },
    ];
    for (i, spans) in rows.into_iter().enumerate() {
        line(frame, spans, b.x, b.y + 3 + i as u16, b.width, b);
    }
}

/// Cor de cada categoria da VRAM (próximas às do gputaskman).
fn vram_color(k: VramKind) -> Color {
    match k {
        VramKind::Game => Color::Rgb(224, 138, 60),
        VramKind::App => Color::Rgb(78, 184, 176),
        VramKind::Compositor => Color::Rgb(157, 124, 216),
        VramKind::Browser => Color::Rgb(92, 156, 230),
        VramKind::System => Color::Rgb(154, 160, 176),
        VramKind::Kernel => Color::Rgb(138, 132, 88),
        VramKind::Free => DIM,
    }
}

/// Partes da barra: o jogo do RTSS sai do "app" e vira a própria categoria.
fn vram_parts(b: &VramBreakdown, game_pid: Option<u32>) -> Vec<(VramKind, u64)> {
    let game: u64 = b
        .processes
        .iter()
        .filter(|p| Some(p.pid) == game_pid && p.kind == VramKind::App)
        .map(|p| p.local)
        .sum();
    let mut parts = Vec::with_capacity(b.segments.len() + 1);
    if game > 0 {
        parts.push((VramKind::Game, game));
    }
    for &(k, v) in &b.segments {
        let v = if k == VramKind::App {
            v.saturating_sub(game)
        } else {
            v
        };
        parts.push((k, v));
    }
    parts
}

/// VRAM: barra cortada por categoria (linha 0, com usado/total da dedicada) e legenda (linha 1).
fn vram_bar(
    frame: &mut Frame<'_>,
    area: Rect,
    b: &VramBreakdown,
    game_pid: Option<u32>,
    dedicated: &Metric<(u64, u64)>,
) {
    if area.height == 0 {
        return;
    }
    let parts = vram_parts(b, game_pid);
    let t = metric(dedicated, |(u, t)| {
        format!(
            " {} / {} ({}%)",
            bytes(*u),
            bytes(*t),
            num(*u as f64 / (*t).max(1) as f64 * 100.0, 0)
        )
    });
    let text_w = (t.width() as u16).min(area.width / 2);
    let label_w = 7u16.min(area.width);
    line(frame, vec![label("VRAM  ")], area.x, area.y, label_w, area);
    let bar_w = area.width.saturating_sub(label_w + text_w);
    let colored: Vec<(u64, Color)> = parts
        .iter()
        .filter(|(k, _)| *k != VramKind::Free)
        .map(|&(k, v)| (v, vram_color(k)))
        .collect();
    frame.render_widget(
        Segmented {
            parts: &colored,
            total: b.total,
        },
        Rect::new(area.x + label_w, area.y, bar_w, 1).intersection(area),
    );
    put(
        frame,
        Paragraph::new(t),
        Rect::new(area.x + label_w + bar_w, area.y, text_w, 1),
        area,
    );
    if area.height < 2 {
        return;
    }
    let mut legend = vec![Span::raw(" ".repeat(usize::from(label_w)))];
    for (k, v) in parts.iter().filter(|(_, v)| *v > 0) {
        legend.push(Span::styled("■ ", Style::new().fg(vram_color(*k))));
        legend.push(label(&format!("{} ", k.label())));
        legend.push(text(format!("{}  ", short_bytes(*v))));
    }
    line(frame, legend, area.x, area.y + 1, area.width, area);
}

/// Linha "rótulo ■■■■ usado/total" dentro de `area`, na linha `row`.
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
    let text_w = (t.width() as u16).min(area.width / 2);
    let label_w = (name.len() as u16 + 1).min(area.width);
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

// --- Memória ---

fn draw_memory(frame: &mut Frame<'_>, area: Rect, st: &mut State) {
    let mem = st.fast.as_ref().map(|f| &f.memory);
    let Some(Ok(m)) = mem else {
        empty_box(frame, area, "mem", mem);
        return;
    };
    let block = boxed("mem", "");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let ram = m.used.clone().and_then(|u| Ok((u, m.total.clone()?)));
    let avail = m
        .used
        .clone()
        .and_then(|u| Ok(m.total.clone()?.saturating_sub(u)));
    let gb = |v: f64| bytes(v as u64);
    line(
        frame,
        vec![
            label("RAM      "),
            metric(&ram, |(u, t)| {
                format!(
                    "{} / {} ({}%)",
                    bytes(*u),
                    bytes(*t),
                    num(*u as f64 / (*t).max(1) as f64 * 100.0, 0)
                )
            }),
            sep(),
            label("avail "),
            metric(&avail, |v| bytes(*v)),
            sep(),
            minmax(st.session.ram, gb),
        ],
        inner.x,
        inner.y,
        inner.width,
        inner,
    );
    meter_line(frame, inner, 1, &ram);
    line(
        frame,
        vec![
            label("commit   "),
            metric(&m.commit, |(c, l)| {
                format!(
                    "{} / {} ({}%)",
                    bytes(*c),
                    bytes(*l),
                    num(*c as f64 / (*l).max(1) as f64 * 100.0, 0)
                )
            }),
            sep(),
            minmax(st.session.commit, gb),
        ],
        inner.x,
        inner.y + 2,
        inner.width,
        inner,
    );
    meter_line(frame, inner, 3, &m.commit);
    let cached = m.cached.clone().and_then(|c| Ok((c, m.total.clone()?)));
    bar_line(frame, inner, 4, "cached  ", &cached);
    let page = m
        .pagefile_usage
        .clone()
        .map(|p| ((p * 100.0) as u64, 10_000));
    if inner.height > 5 {
        let y = inner.y + 5;
        line(frame, vec![label("pagefile ")], inner.x, y, 9, inner);
        let t = metric(&m.pagefile_usage, |v| format!(" {}%", num(*v, 1)));
        let tw = (t.width() as u16).min(inner.width / 2);
        let bw = inner.width.saturating_sub(9 + tw);
        if let Ok((u, t)) = page {
            frame.render_widget(
                Meter {
                    value: u as f64 / t as f64,
                },
                Rect::new(inner.x + 9, y, bw, 1).intersection(inner),
            );
        }
        put(
            frame,
            Paragraph::new(t),
            Rect::new(inner.x + 9 + bw, y, tw, 1),
            inner,
        );
    }
    line(
        frame,
        vec![
            label("kernel pools   paged "),
            metric(&m.paged_pool, |v| bytes(*v)),
            label("   non-paged "),
            metric(&m.nonpaged_pool, |v| bytes(*v)),
        ],
        inner.x,
        inner.y + 7,
        inner.width,
        inner,
    );
}

/// Barra cheia de largura total na linha `row` (RAM e confirmada, abaixo do texto).
fn meter_line(frame: &mut Frame<'_>, area: Rect, row: u16, m: &Metric<(u64, u64)>) {
    if row >= area.height {
        return;
    }
    if let Ok((u, t)) = m {
        let v = if *t > 0 { *u as f64 / *t as f64 } else { 0.0 };
        frame.render_widget(
            Meter { value: v },
            Rect::new(area.x, area.y + row, area.width, 1).intersection(area),
        );
    }
}

// --- Rede ---

fn draw_net(frame: &mut Frame<'_>, area: Rect, st: &mut State) {
    let nics = st.fast.as_ref().map(|f| &f.nics);
    let Some(Ok(list)) = nics else {
        empty_box(frame, area, "net", nics);
        return;
    };
    let list: Vec<Nic> = list.clone();
    let block = boxed("net", "mirrored ↑↓");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // Só o estado neutro ("disconnected") vai para a linha de desconectadas; falha aparece.
    let (down, up): (Vec<_>, Vec<_>) = list
        .iter()
        .partition(|n| matches!(n.link, Err(Missing::Neutral(_))));
    let reserved = u16::from(!down.is_empty());
    let bottom = inner.y + inner.height.saturating_sub(reserved);
    let per = (inner.height.saturating_sub(reserved) / (up.len().max(1) as u16)).max(3);
    const RATE_W: u16 = 34;
    let mut y = inner.y;
    for n in up {
        if y + 2 > bottom {
            break;
        }
        // Cabeçalho: alias (descrição) à esquerda, link à direita.
        line(
            frame,
            vec![
                text(printable(&n.alias)).add_modifier(Modifier::BOLD),
                label(&format!("  ({})", printable(&n.description))),
            ],
            inner.x,
            y,
            inner.width.saturating_sub(18),
            inner,
        );
        put(
            frame,
            Paragraph::new(Line::from(vec![
                label("link "),
                metric(&n.link, |v| bits(*v as f64)),
            ]))
            .right_aligned(),
            Rect::new(inner.x, y, inner.width, 1),
            inner,
        );
        let body = (per - 1).min(bottom.saturating_sub(y + 1));
        let half_up = body.div_ceil(2);
        let half_down = body - half_up;
        let gw = inner.width.saturating_sub(RATE_W + 1);
        let n_samples = usize::from(gw) * 2;
        for (k, h, series, m, color, arrow, mm) in [
            (
                0u16,
                half_up,
                st.send.get_mut(&n.alias),
                &n.send,
                NET_SEND,
                "↑",
                st.session.send.get(&n.alias).copied(),
            ),
            (
                half_up,
                half_down,
                st.recv.get_mut(&n.alias),
                &n.recv,
                NET_RECV,
                "↓",
                st.session.recv.get(&n.alias).copied(),
            ),
        ] {
            if h == 0 {
                continue;
            }
            let row = y + 1 + k;
            let scale = series
                .as_ref()
                .map_or(NET_FLOOR, |s| s.max_of_ref(n_samples))
                .max(NET_FLOOR);
            if let Some(s) = series {
                let data = s.over(n_samples, scale);
                let g = Graph::new(&data, GraphColor::Fixed(color));
                let g = if arrow == "↓" { g.inverted() } else { g };
                frame.render_widget(g, Rect::new(inner.x, row, gw, h).intersection(inner));
            }
            let mut spans = vec![
                Span::styled(format!(" {arrow} "), Style::new().fg(color)),
                metric(m, |v| bits(*v)),
            ];
            if let Some((_, hi)) = mm.and_then(MinMax::get) {
                spans.push(label(&format!("  max {}", bits(hi))));
            }
            line(frame, spans, inner.x + gw + 1, row, RATE_W, inner);
            if h > 1 {
                line(
                    frame,
                    vec![label(&format!("   scale {}", bits(scale)))],
                    inner.x + gw + 1,
                    row + 1,
                    RATE_W,
                    inner,
                );
            }
        }
        y += per;
    }
    if !down.is_empty() {
        let names: Vec<String> = down.iter().map(|n| printable(&n.alias)).collect();
        line(
            frame,
            vec![
                label("disconnected: "),
                Span::styled(names.join(", "), Style::new().fg(NEUTRAL)),
            ],
            inner.x,
            inner.y + inner.height - 1,
            inner.width,
            inner,
        );
    }
}

impl Series {
    /// `max_of` sem exigir `&mut` (para o cálculo da escala antes do gráfico).
    fn max_of_ref(&self, n: usize) -> f64 {
        let len = self.0.len();
        self.0
            .iter()
            .skip(len.saturating_sub(n))
            .copied()
            .fold(0.0, f64::max)
    }
}

// --- Discos ---

/// Ordem de exibição: discos com letra e o virtual; membros do pool depois (recolhidos se
/// `fold`). Devolve (disco, é membro).
fn disk_order(st: &State) -> Vec<(Disk, bool)> {
    let Some(Ok(list)) = st.fast.as_ref().map(|f| &f.disks) else {
        return Vec::new();
    };
    let mut out: Vec<(Disk, bool)> = list
        .iter()
        .filter(|d| d.pool_member != Ok(true))
        .map(|d| (d.clone(), false))
        .collect();
    out.sort_by_key(|(d, _)| d.letters.first().cloned().unwrap_or_else(|| "~".into()));
    if !pool_folded(st, list.len()) {
        out.extend(
            list.iter()
                .filter(|d| d.pool_member == Ok(true))
                .map(|d| (d.clone(), true)),
        );
    }
    out
}

/// Recolhe os membros do pool por escolha (`d`) ou, no automático, se não couberem.
fn pool_folded(st: &State, total: usize) -> bool {
    st.fold_pool.unwrap_or(total > 12)
}

fn draw_disks(frame: &mut Frame<'_>, area: Rect, st: &mut State) {
    let disks = st.fast.as_ref().map(|f| &f.disks);
    let Some(Ok(all)) = disks else {
        empty_box(frame, area, "disks", disks);
        return;
    };
    let members = all.iter().filter(|d| d.pool_member == Ok(true)).count();
    let order = disk_order(st);
    let folded = pool_folded(st, all.len());
    let block = boxed(
        "disks",
        &format!(
            "S=sys P=page{}",
            if members > 0 { "  [d] fold pool" } else { "" }
        ),
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let temps = st.slow.as_ref().map(|s| &s.disk_temps);
    const NAME_W: u16 = 26;
    const INFO_W: u16 = 54;
    // Depois do nome: barramento (7), tipo (4), capacidade (6 + espaço), flags (4) e 2 espaços.
    const LEFT_W: u16 = 24;
    let spark_w = inner.width.saturating_sub(NAME_W + LEFT_W + INFO_W);
    // Cabeçalho nas mesmas posições das colunas de cada linha.
    let info_x = inner.x + inner.width.saturating_sub(INFO_W);
    line(
        frame,
        vec![label(&format!(
            "{:<26}{:<7}{:<4}{:>6} {:<4}  ",
            "drive / model", "bus", "typ", "cap", "flg"
        ))],
        inner.x,
        inner.y,
        NAME_W + LEFT_W,
        inner,
    );
    if spark_w > 0 {
        line(
            frame,
            vec![label("active")],
            inner.x + NAME_W + LEFT_W,
            inner.y,
            spark_w,
            inner,
        );
    }
    line(
        frame,
        vec![label(&format!(
            "{:>5}{:>8}{:>11}{:>11}  temp",
            "act", "resp", "read", "write"
        ))],
        info_x,
        inner.y,
        INFO_W,
        inner,
    );
    let rows = inner.height.saturating_sub(1);
    for (i, (d, member)) in order.iter().enumerate() {
        let y = inner.y + 1 + i as u16;
        if i as u16 >= rows {
            // Não cabe: avisa quantos ficaram de fora (nada some sem aviso).
            line(
                frame,
                vec![label(&format!("+{} more", order.len() - i))],
                inner.x,
                y - 1,
                inner.width,
                inner,
            );
            break;
        }
        let letters = if d.letters.is_empty() {
            format!("d{}", d.number)
        } else {
            d.letters.join(" ")
        };
        let model = d
            .model
            .as_ref()
            .map_or_else(ToString::to_string, |m| printable(m));
        let mut name = if *member {
            format!("  └ {letters} {model}")
        } else {
            format!("{letters} {model}")
        };
        if !member
            && folded
            && d.bus
                .as_ref()
                .is_ok_and(|b| matches!(b, crate::model::Bus::Spaces))
            && members > 0
        {
            name = format!("{letters} {model} ({members} members)");
        }
        let name: String = if name.chars().count() > usize::from(NAME_W) - 1 {
            let mut s: String = name.chars().take(usize::from(NAME_W) - 2).collect();
            s.push('…');
            s
        } else {
            name
        };
        let bus = d.bus.as_ref().map_or("?".into(), ToString::to_string);
        let typ = d
            .ssd
            .as_ref()
            .map_or("?", |s| if *s { "SSD" } else { "HDD" });
        let cap = d.capacity.as_ref().map_or("?".into(), |c| short_bytes(*c));
        let flags = format!(
            "{}{}",
            if d.system { "S " } else { "" },
            if d.pagefile { "P" } else { "" }
        );
        line(
            frame,
            vec![
                Span::styled(
                    format!("{name:<26}"),
                    Style::new().fg(if *member { LABEL } else { TEXT }),
                ),
                label(&format!("{bus:<7}{typ:<4}{cap:>6} {flags:<4}  ")),
            ],
            inner.x,
            y,
            NAME_W + LEFT_W,
            inner,
        );
        if spark_w > 0
            && let Some(s) = st.disks.get_mut(&d.number)
        {
            frame.render_widget(
                Graph::new(s.values(), GraphColor::Value),
                Rect::new(inner.x + NAME_W + LEFT_W, y, spark_w, 1).intersection(inner),
            );
        }
        let same = |t: &&crate::model::DiskTemp| t.model == d.model.as_ref().ok().cloned();
        let t = match temps {
            Some(Ok(list)) => list
                .iter()
                .find(|t| t.number == d.number)
                .filter(same)
                .map_or_else(|| label(""), |t| metric(&t.celsius, |c| format!("{c} °C"))),
            Some(Err(e)) => missing(e),
            None => label(""),
        };
        let info = vec![
            metric(&d.active, |v| format!("{:>4}%", num(*v, 0))),
            metric(&d.response, |v| format!(" {:>5}ms", num(v * 1000.0, 1))),
            metric(&d.read, |v| {
                format!(" {:>10}", format!("{}/s", short_bytes(*v as u64)))
            }),
            metric(&d.write, |v| {
                format!(" {:>10}", format!("{}/s", short_bytes(*v as u64)))
            }),
            Span::raw("  "),
            t,
        ];
        let x = inner.x + inner.width.saturating_sub(INFO_W);
        line(frame, info, x, y, INFO_W, inner);
    }
}

// --- Jogo ---

/// Teto do gráfico de FPS: o menor degrau acima do máximo visível.
fn fps_ceiling(max: f64) -> f64 {
    FPS_STEPS
        .iter()
        .copied()
        .find(|s| *s >= max)
        .unwrap_or(FPS_STEPS[FPS_STEPS.len() - 1])
}

fn draw_game(frame: &mut Frame<'_>, area: Rect, st: &mut State) {
    let Some(fps) = st.fps.clone() else {
        waiting(frame, area, boxed("game", ""));
        return;
    };
    let pipe_failed = matches!(fps.pipe, Err(Missing::Fail(_)));
    let g = match &fps.game {
        Ok(g) => g,
        Err(e) => {
            let mut lines = vec![
                Line::from(missing(e)),
                Line::from(vec![label("RTSS "), metric(&fps.rtss, Clone::clone)]),
            ];
            if pipe_failed {
                lines.push(Line::from(vec![
                    label("frametime pipe "),
                    metric(&fps.pipe, |_| String::new()),
                ]));
            }
            frame.render_widget(Paragraph::new(lines).block(boxed("game", "")), area);
            return;
        }
    };
    let res = g
        .resolution
        .as_ref()
        .map_or(String::new(), |(x, y)| format!(" · {x}x{y}"));
    let title = format!(
        "game · {} ({} · PID {}{res})",
        printable(&g.name),
        g.api,
        g.pid
    );
    let source = match (g.approx, pipe_failed) {
        (false, _) => "pipe: exact",
        (true, false) => "approx. (game opened before wintop)",
        (true, true) => "approx. (frametime pipe failed)",
    };
    let block = boxed(&title, source);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let info_h = 2 + u16::from(pipe_failed);
    let [graphs, info] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(info_h)]).areas(inner);
    let [fps_area, ft_area] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(graphs);

    // FPS: teto em degraus pelo máximo visível.
    let n = usize::from(fps_area.width) * 2;
    let ceil = fps_ceiling(st.game_fps.max_of(n));
    let head = vec![
        label("FPS "),
        metric(&g.fps, |v| num(*v, 0)).add_modifier(Modifier::BOLD),
        sep(),
        minmax(st.session.fps, |v| num(v, 0)),
    ];
    line(
        frame,
        head,
        fps_area.x,
        fps_area.y,
        fps_area.width.saturating_sub(6),
        fps_area,
    );
    put(
        frame,
        Paragraph::new(label(&format!("{} ", num(ceil, 0)))).right_aligned(),
        Rect::new(fps_area.x, fps_area.y, fps_area.width, 1),
        fps_area,
    );
    let data = st.game_fps.over(n, ceil);
    let gr = Rect::new(
        fps_area.x,
        fps_area.y + 1,
        fps_area.width.saturating_sub(1),
        fps_area.height.saturating_sub(1),
    );
    frame.render_widget(
        Graph::new(&data, GraphColor::Fixed(FPS_COLOR)),
        gr.intersection(fps_area),
    );

    // Frametime: teto fixo de 50 ms; picos acima de 33 ms em vermelho.
    let n = usize::from(ft_area.width) * 2;
    let head = vec![
        label("FT "),
        metric(&g.frametime, |(a, _, _)| format!("{} ms", num(*a, 2))).add_modifier(Modifier::BOLD),
        sep(),
        minmax(st.session.ft, |v| num(v, 1)),
    ];
    line(
        frame,
        head,
        ft_area.x + 1,
        ft_area.y,
        ft_area.width.saturating_sub(7),
        ft_area,
    );
    put(
        frame,
        Paragraph::new(label("50 ms ")).right_aligned(),
        Rect::new(ft_area.x, ft_area.y, ft_area.width, 1),
        ft_area,
    );
    let data = st.game_ft.over(n, FT_CEIL_MS);
    let gr = Rect::new(
        ft_area.x + 1,
        ft_area.y + 1,
        ft_area.width.saturating_sub(1),
        ft_area.height.saturating_sub(1),
    );
    frame.render_widget(
        Graph::new(
            &data,
            GraphColor::Threshold {
                base: FT_COLOR,
                alert: FAIL,
                at: FT_ALERT_MS / FT_CEIL_MS,
            },
        ),
        gr.intersection(ft_area),
    );

    let ms = |v: f64| format!("{} ms", num(v, 2));
    let mut lines = vec![
        Line::from(vec![
            label("1% low "),
            metric(&g.lows, |(a, _)| num(*a, 0)),
            label("   0.1% low "),
            metric(&g.lows, |(_, b)| num(*b, 0)),
            bar_sep(),
            label("frametime "),
            metric(&g.frametime, |(a, lo, hi)| {
                format!("{} ({} - {} ms)", ms(*a), num(*lo, 1), num(*hi, 1))
            }),
            bar_sep(),
            label("GPU busy "),
            metric(&g.gpu_time, |(a, f)| {
                format!("{} / frame {}", ms(*a), ms(*f))
            }),
        ]),
        Line::from(vec![
            label("limited by "),
            metric(&g.bound, ToString::to_string),
            bar_sep(),
            label("lows over 60 s"),
            bar_sep(),
            label(&format!("graphs synced to the {} ms tick", st.interval_ms)),
        ]),
    ];
    // Falha do pipe (outro servidor ocupando o nome): linha própria, só quando acontece.
    if pipe_failed {
        lines.push(Line::from(vec![
            label("frametime pipe "),
            metric(&fps.pipe, |_| String::new()),
        ]));
    }
    put(frame, Paragraph::new(lines), info, info);
}
