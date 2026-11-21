//! Aba Geral: CPU, GPU, memória, discos, rede e jogo. Desenho puro a partir do estado (testado
//! com snapshots do `insta`).
//!
//! Direção do usuário: uso (CPU, GPU, disco, rede) em gráfico; RAM e VRAM em barra;
//! temperaturas só como número.

use std::collections::{HashMap, VecDeque};

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};

use super::widgets::{DIM, Graph, Meter, gradient};
use crate::fmt::{bits, bytes, num};
use crate::model::{Disk, Fast, Fps, Metric, Missing, Slow};
use crate::win::printable;

/// Amostras guardadas por série (cobre ~500 colunas de braille).
const HISTORY: usize = 1000;

const TITLE: Color = Color::Rgb(220, 220, 220);
const TEXT: Color = Color::Rgb(200, 200, 200);
const LABEL: Color = Color::Rgb(130, 130, 145);
const NEUTRAL: Color = Color::Rgb(120, 120, 120);
const FAIL: Color = Color::Rgb(235, 80, 80);
const BORDER_CPU: Color = Color::Rgb(90, 160, 110);
const BORDER_GPU: Color = Color::Rgb(110, 140, 200);
const BORDER_MEM: Color = Color::Rgb(190, 160, 90);
const BORDER_DISK: Color = Color::Rgb(170, 120, 190);
const BORDER_NET: Color = Color::Rgb(90, 170, 180);
const BORDER_GAME: Color = Color::Rgb(200, 130, 100);
const NET_SEND: Color = Color::Rgb(200, 120, 200);
const NET_RECV: Color = Color::Rgb(110, 190, 230);

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

    /// Valores divididos pelo maior entre os últimos `n` (escala automática da rede).
    fn scaled(&mut self, n: usize) -> Vec<f64> {
        let v = self.values();
        let tail = &v[v.len().saturating_sub(n)..];
        let max = tail.iter().copied().fold(0.0, f64::max);
        tail.iter()
            .map(|x| if max > 0.0 { x / max } else { 0.0 })
            .collect()
    }
}

/// Estado da interface: os últimos dados e o histórico dos gráficos.
#[derive(Default)]
pub struct State {
    pub fast: Option<Fast>,
    pub slow: Option<Slow>,
    pub fps: Option<Fps>,
    pub interval_ms: u64,
    cpu: Series,
    gpu: Series,
    disks: HashMap<u32, Series>,
    send: HashMap<String, Series>,
    recv: HashMap<String, Series>,
    game_fps: Series,
    game_pid: Option<u32>,
}

impl State {
    pub fn new(interval_ms: u64) -> Self {
        Self {
            interval_ms,
            ..Default::default()
        }
    }

    /// Nova coleta da thread rápida: guarda e alimenta os históricos.
    pub fn push_fast(&mut self, f: Fast) {
        let usage = |m: &Metric<f64>| m.as_ref().map_or(0.0, |v| v / 100.0);
        if let Ok(c) = &f.cpu {
            self.cpu.push(usage(&c.usage));
        }
        if let Ok(g) = &f.gpus
            && let Some(g) = g.first()
        {
            self.gpu
                .push(g.usage.as_ref().map_or(0.0, |v| f64::from(*v) / 100.0));
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
            }
        }
        self.fast = Some(f);
    }

    /// Novo estado do RTSS (a cada 250 ms com jogo).
    pub fn push_fps(&mut self, s: Fps) {
        if let Ok(g) = &s.game {
            if self.game_pid != Some(g.pid) {
                self.game_pid = Some(g.pid);
                self.game_fps = Series::default();
            }
            self.game_fps.push(g.fps.as_ref().map_or(0.0, |v| *v));
        }
        self.fps = Some(s);
    }
}

/// Texto de uma métrica: o valor, o estado neutro (cinza) ou a falha (vermelho, com a causa).
fn metric<T>(m: &Metric<T>, f: impl FnOnce(&T) -> String) -> Span<'static> {
    match m {
        Ok(v) => Span::styled(f(v), Style::new().fg(TEXT)),
        Err(Missing::Neutral(s)) => Span::styled(s.clone(), Style::new().fg(NEUTRAL)),
        Err(Missing::Fail(s)) => Span::styled(format!("! {s}"), Style::new().fg(FAIL)),
    }
}

fn label(s: &str) -> Span<'static> {
    Span::styled(s.to_string(), Style::new().fg(LABEL))
}

fn sep() -> Span<'static> {
    Span::styled("  ", Style::new())
}

fn boxed(title: &str, color: Color) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(color))
        .title(Span::styled(
            format!(" {title} "),
            Style::new().fg(TITLE).add_modifier(Modifier::BOLD),
        ))
}

fn temp(m: &Metric<f64>) -> Span<'static> {
    metric(m, |v| format!("{} °C", num(*v, 1)))
}

pub fn draw(frame: &mut Frame<'_>, st: &mut State) {
    let [body, footer] =
        Layout::vertical([Constraint::Min(10), Constraint::Length(1)]).areas(frame.area());
    let [top, middle, bottom] = Layout::vertical([
        Constraint::Percentage(40),
        Constraint::Percentage(32),
        Constraint::Percentage(28),
    ])
    .areas(body);
    let [cpu, gpu] =
        Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).areas(top);
    let [mem, disks] =
        Layout::horizontal([Constraint::Percentage(32), Constraint::Percentage(68)]).areas(middle);
    let [net, game] =
        Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).areas(bottom);
    draw_cpu(frame, cpu, st);
    draw_gpu(frame, gpu, st);
    draw_memory(frame, mem, st);
    draw_disks(frame, disks, st);
    draw_net(frame, net, st);
    draw_game(frame, game, st);
    draw_footer(frame, footer, st);
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

fn draw_cpu(frame: &mut Frame<'_>, area: Rect, st: &mut State) {
    let Some(Ok(c)) = st.fast.as_ref().map(|f| &f.cpu) else {
        let block = boxed("cpu", BORDER_CPU);
        if let Some(Err(e)) = st.fast.as_ref().map(|f| &f.cpu) {
            let span = metric::<()>(&Err(e.clone()), |()| String::new());
            frame.render_widget(Paragraph::new(span).block(block), area);
        } else {
            waiting(frame, area, block);
        }
        return;
    };
    let name = c.name.as_ref().map_or("cpu".to_string(), |n| printable(n));
    let block = boxed(&format!("cpu  {name}"), BORDER_CPU);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [graph_area, info] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(2)]).areas(inner);
    let per = c.per_logical.as_ref().ok();
    // Grade dos lógicos à direita: duas colunas de largura fixa (com a causa, se falhou).
    let cores_w = 34;
    let [graph, cores] =
        Layout::horizontal([Constraint::Min(10), Constraint::Length(cores_w)]).areas(graph_area);
    frame.render_widget(
        Graph {
            samples: st.cpu.values(),
            color: None,
        },
        graph,
    );

    frame.render_widget(
        Paragraph::new(Line::from(vec![metric(&c.usage, |v| {
            format!("{}%", num(*v, 0))
        })])),
        Rect::new(graph.x, graph.y, 6, 1).intersection(graph),
    );

    if let Some(per) = per {
        // Duas colunas equilibradas (16 lógicos = 8 + 8); o que não couber na altura fica de fora.
        let rows = per.len().div_ceil(2).max(1);
        for (i, v) in per.iter().enumerate() {
            let (col, row) = (i / rows, i % rows);
            if row >= usize::from(cores.height) {
                continue;
            }
            let x = cores.x + 1 + col as u16 * 17;
            let y = cores.y + row as u16;
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    label(&format!("C{i:<2}")),
                    Span::styled(format!("{:>4}%", num(*v, 0)), Style::new().fg(TEXT)),
                ])),
                Rect::new(x, y, 9, 1).intersection(cores),
            );
            frame.render_widget(
                Meter { value: v / 100.0 },
                Rect::new(x + 10, y, 6, 1).intersection(cores),
            );
        }
    } else {
        // Uso por lógico indisponível: a causa aparece no lugar da grade.
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                label("per core "),
                metric(&c.per_logical, |_| String::new()),
            ])),
            cores,
        );
    }
    let sensors = st.fast.as_ref().map(|f| &f.cpu_sensors);
    let mut l1 = vec![
        label("speed "),
        metric(&c.freq_mhz, |v| format!("{} GHz", num(v / 1000.0, 2))),
        sep(),
        label("kernel "),
        metric(&c.kernel, |v| format!("{}%", num(*v, 0))),
    ];
    match sensors {
        Some(Ok(s)) => {
            l1.extend([sep(), label("Tctl "), temp(&s.tctl)]);
            match &s.ccds {
                Ok(list) => {
                    for (n, t) in list {
                        l1.extend([sep(), label(&format!("CCD{n} ")), temp(&Ok(*t))]);
                    }
                }
                Err(e) => l1.extend([
                    sep(),
                    label("CCD "),
                    metric::<()>(&Err(e.clone()), |()| String::new()),
                ]),
            }
            l1.extend([
                sep(),
                label("power "),
                metric(&s.package_power, |v| format!("{} W", num(*v, 1))),
            ]);
        }
        Some(Err(e)) => l1.extend([
            sep(),
            label("sensors "),
            metric::<()>(&Err(e.clone()), |()| String::new()),
        ]),
        None => {}
    }
    let up = c.uptime.as_secs();
    let l2 = vec![
        label("up "),
        Span::styled(
            format!("{}d {:02}:{:02}", up / 86400, up / 3600 % 24, up / 60 % 60),
            Style::new().fg(TEXT),
        ),
        sep(),
        metric(&c.counts, |n| {
            format!(
                "{} proc  {} thr  {} handles",
                n.processes, n.threads, n.handles
            )
        }),
        sep(),
        metric(&c.base_mhz, |m| {
            format!("base {} GHz", num(f64::from(*m) / 1000.0, 2))
        }),
    ];
    frame.render_widget(Paragraph::new(vec![Line::from(l1), Line::from(l2)]), info);
}

fn draw_gpu(frame: &mut Frame<'_>, area: Rect, st: &mut State) {
    let gpus = st.fast.as_ref().map(|f| &f.gpus);
    let Some(Ok(list)) = gpus else {
        let block = boxed("gpu", BORDER_GPU);
        match gpus {
            Some(Err(e)) => frame.render_widget(
                Paragraph::new(metric::<()>(&Err(e.clone()), |()| String::new())).block(block),
                area,
            ),
            _ => waiting(frame, area, block),
        }
        return;
    };
    let Some(g) = list.first() else {
        frame.render_widget(
            Paragraph::new(metric::<()>(
                &Err(Missing::Fail("NVML reported no GPU".into())),
                |()| String::new(),
            ))
            .block(boxed("gpu", BORDER_GPU)),
            area,
        );
        return;
    };
    let name = g.name.as_ref().map_or("gpu".to_string(), |n| printable(n));
    let block = boxed(&format!("gpu  {name}"), BORDER_GPU);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [graph, bars, info] = Layout::vertical([
        Constraint::Min(2),
        Constraint::Length(2),
        Constraint::Length(3),
    ])
    .areas(inner);
    frame.render_widget(
        Graph {
            samples: st.gpu.values(),
            color: None,
        },
        graph,
    );
    frame.render_widget(
        Paragraph::new(metric(&g.usage, |v| format!("{v}%"))),
        Rect::new(graph.x, graph.y, 6, 1).intersection(graph),
    );
    bar_line(frame, bars, 0, "VRAM  ", &g.dedicated);
    bar_line(frame, bars, 1, "shared", &g.shared);
    let clock = |m: &Metric<(u32, u32)>| metric(m, |(c, _)| format!("{c} MHz"));
    let slow_gpus = st.slow.as_ref().map(|s| &s.gpus);
    let slow = slow_gpus
        .and_then(|r| r.as_ref().ok())
        .and_then(|l| l.iter().find(|x| x.index == g.index));
    let mut l3 = vec![
        label("PCIe "),
        metric(&g.pcie_link, |(generation, w)| {
            format!("Gen{generation} x{w}")
        }),
    ];
    if let Some(Err(e)) = slow_gpus {
        l3.extend([
            sep(),
            label("tx/rx "),
            metric::<()>(&Err(e.clone()), |()| String::new()),
        ]);
    }
    if let Some(s) = slow {
        l3.extend([
            sep(),
            label("tx "),
            metric(&s.pcie_tx, |v| format!("{}/s", bytes(*v))),
            sep(),
            label("rx "),
            metric(&s.pcie_rx, |v| format!("{}/s", bytes(*v))),
        ]);
    }
    let fans: Vec<Span<'static>> = match &g.fans {
        Ok(list) => list
            .iter()
            .enumerate()
            .flat_map(|(i, f)| {
                [
                    label(&format!("{}fan{i} ", if i == 0 { "" } else { " " })),
                    metric(&f.percent, |v| format!("{v}%")),
                    Span::raw(" "),
                    metric(&f.rpm, |v| format!("{v} rpm")),
                ]
            })
            .collect(),
        Err(e) => vec![
            label(" fans "),
            metric::<()>(&Err(e.clone()), |()| String::new()),
        ],
    };
    let l1 = vec![
        label("temp "),
        metric(&g.temp, |v| format!("{v} °C")),
        sep(),
        label("power "),
        metric(&g.power, |(p, l)| {
            format!("{}/{} W", num(*p, 0), num(*l, 0))
        }),
        sep(),
        label("core "),
        clock(&g.clock_graphics),
        sep(),
        label("mem "),
        clock(&g.clock_mem),
    ];
    // Fans na 2ª linha (na 1ª o 2º fan era cortado na largura da caixa).
    let mut l2 = fans;
    l2.push(sep());
    l2.extend(vec![
        label("enc "),
        metric(&g.encoder, |v| format!("{v}%")),
        sep(),
        label("dec "),
        metric(&g.decoder, |v| format!("{v}%")),
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
    ]);
    frame.render_widget(
        Paragraph::new(vec![Line::from(l1), Line::from(l2), Line::from(l3)]),
        info,
    );
}

/// Linha "rótulo ■■■■ usado/total" dentro de `area`, na linha `row`.
fn bar_line(frame: &mut Frame<'_>, area: Rect, row: u16, name: &str, m: &Metric<(u64, u64)>) {
    if row >= area.height {
        return;
    }
    let y = area.y + row;
    let text = metric(m, |(u, t)| format!(" {} / {}", bytes(*u), bytes(*t)));
    let text_w = (text.width() as u16).min(area.width / 2);
    let label_w = name.len() as u16 + 1;
    frame.render_widget(
        Paragraph::new(label(name)),
        Rect::new(area.x, y, label_w, 1).intersection(area),
    );
    let bar_w = area.width.saturating_sub(label_w + text_w);
    if let Ok((u, t)) = m {
        let v = if *t > 0 { *u as f64 / *t as f64 } else { 0.0 };
        frame.render_widget(
            Meter { value: v },
            Rect::new(area.x + label_w, y, bar_w, 1).intersection(area),
        );
    }
    frame.render_widget(
        Paragraph::new(text),
        Rect::new(area.x + label_w + bar_w, y, text_w, 1).intersection(area),
    );
}

fn draw_memory(frame: &mut Frame<'_>, area: Rect, st: &mut State) {
    let block = boxed("mem", BORDER_MEM);
    let mem = st.fast.as_ref().map(|f| &f.memory);
    let Some(Ok(m)) = mem else {
        match mem {
            Some(Err(e)) => frame.render_widget(
                Paragraph::new(metric::<()>(&Err(e.clone()), |()| String::new())).block(block),
                area,
            ),
            _ => waiting(frame, area, block),
        }
        return;
    };
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let ram = m.used.clone().and_then(|u| Ok((u, m.total.clone()?)));
    bar_line(frame, inner, 0, "RAM   ", &ram);
    bar_line(frame, inner, 1, "commit", &m.commit);
    let lines = vec![
        Line::from(vec![label("cached    "), metric(&m.cached, |v| bytes(*v))]),
        Line::from(vec![
            label("paged     "),
            metric(&m.paged_pool, |v| bytes(*v)),
            sep(),
            label("non-paged "),
            metric(&m.nonpaged_pool, |v| bytes(*v)),
        ]),
        Line::from(vec![
            label("page file "),
            metric(&m.pagefile_usage, |v| format!("{}%", num(*v, 1))),
        ]),
    ];
    let rest = Rect::new(
        inner.x,
        inner.y + 3,
        inner.width,
        inner.height.saturating_sub(3),
    )
    .intersection(inner);
    frame.render_widget(Paragraph::new(lines), rest);
}

fn disk_name(d: &Disk) -> String {
    let letters = if d.letters.is_empty() {
        format!("disk {}", d.number)
    } else {
        d.letters.join(" ")
    };
    let model = d.model.as_ref().map_or(String::new(), |m| printable(m));
    format!("{letters} {model}")
}

fn draw_disks(frame: &mut Frame<'_>, area: Rect, st: &mut State) {
    let block = boxed("disks", BORDER_DISK);
    let disks = st.fast.as_ref().map(|f| &f.disks);
    let Some(Ok(list)) = disks else {
        match disks {
            Some(Err(e)) => frame.render_widget(
                Paragraph::new(metric::<()>(&Err(e.clone()), |()| String::new())).block(block),
                area,
            ),
            _ => waiting(frame, area, block),
        }
        return;
    };
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let temps_all = st.slow.as_ref().map(|s| &s.disk_temps);
    let temps = temps_all.and_then(|r| r.as_ref().ok());
    let temps_err = match temps_all {
        Some(Err(e)) => Some(e.clone()),
        _ => None,
    };
    // Discos com letra (e o disco virtual) primeiro; membros do pool por último, indentados.
    let mut order: Vec<&Disk> = list.iter().filter(|d| d.pool_member != Ok(true)).collect();
    order.extend(list.iter().filter(|d| d.pool_member == Ok(true)));
    const NAME_W: u16 = 30;
    const INFO_W: u16 = 40;
    // Altura de cada disco: o espaço dividido entre eles, de 1 a 3 linhas de gráfico.
    let per_disk = (inner.height / (order.len().max(1) as u16)).clamp(1, 3);
    for (row, d) in order.iter().enumerate() {
        let y = inner.y + row as u16 * per_disk;
        if y >= inner.y + inner.height {
            break;
        }
        let rows = per_disk.min(inner.y + inner.height - y);
        // Nome e números na linha de baixo da faixa, alinhados com a base do gráfico.
        let text_y = y + rows - 1;
        let member = d.pool_member == Ok(true);
        let mut name = disk_name(d);
        if member {
            name = format!("  └ {name}");
        }
        let name_w = NAME_W.min(inner.width);
        frame.render_widget(
            Paragraph::new(Span::styled(
                truncate(&name, usize::from(name_w)),
                Style::new().fg(if member { LABEL } else { TEXT }),
            )),
            Rect::new(inner.x, text_y, name_w, 1).intersection(inner),
        );
        let graph_w = inner.width.saturating_sub(NAME_W + INFO_W + 1);
        if graph_w > 0
            && let Some(s) = st.disks.get_mut(&d.number)
        {
            frame.render_widget(
                Graph {
                    samples: s.values(),
                    color: None,
                },
                Rect::new(inner.x + NAME_W, y, graph_w, rows).intersection(inner),
            );
        }
        let same_model = |t: &&crate::model::DiskTemp| t.model == d.model.as_ref().ok().cloned();
        let t = temps
            .and_then(|l| l.iter().find(|t| t.number == d.number))
            .filter(same_model)
            .map_or_else(
                || {
                    temps_err.clone().map_or_else(
                        || Span::raw(""),
                        |e| metric::<()>(&Err(e), |()| String::new()),
                    )
                },
                |t| metric(&t.celsius, |c| format!("{c} °C")),
            );
        let info = Line::from(vec![
            metric(&d.active, |v| format!("{:>3}%", num(*v, 0))),
            label(" r "),
            metric(&d.read, |v| {
                format!("{:>9}", format!("{}/s", bytes(*v as u64)))
            }),
            label(" w "),
            metric(&d.write, |v| {
                format!("{:>9}", format!("{}/s", bytes(*v as u64)))
            }),
            Span::raw(" "),
            t,
        ]);
        let x = inner.x + inner.width.saturating_sub(INFO_W);
        frame.render_widget(
            Paragraph::new(info),
            Rect::new(x, text_y, INFO_W.min(inner.width), 1).intersection(inner),
        );
    }
}

fn truncate(s: &str, w: usize) -> String {
    if s.chars().count() <= w {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(w.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

fn draw_net(frame: &mut Frame<'_>, area: Rect, st: &mut State) {
    let block = boxed("net", BORDER_NET);
    let nics = st.fast.as_ref().map(|f| &f.nics);
    let Some(Ok(list)) = nics else {
        match nics {
            Some(Err(e)) => frame.render_widget(
                Paragraph::new(metric::<()>(&Err(e.clone()), |()| String::new())).block(block),
                area,
            ),
            _ => waiting(frame, area, block),
        }
        return;
    };
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // Interfaces conectadas com gráfico (2 linhas: envio e recebimento); desconectadas numa
    // linha só, no fim.
    let (down, up): (Vec<_>, Vec<_>) = list
        .iter()
        .partition(|n| matches!(n.link, Err(Missing::Neutral(_))));
    let mut y = inner.y;
    let bottom = inner.y + inner.height;
    const NAME_W: u16 = 24;
    const RATE_W: u16 = 14;
    let reserved = u16::from(!down.is_empty());
    // Cada interface ganha a mesma altura: envio e recebimento com 1 a 4 linhas cada.
    let per_nic = (inner.height.saturating_sub(reserved) / (up.len().max(1) as u16)).max(2);
    let half = (per_nic / 2).clamp(1, 4);
    for n in up {
        if y + 1 >= bottom {
            break;
        }
        let head = Line::from(vec![Span::styled(
            truncate(&printable(&n.alias), usize::from(NAME_W) - 1),
            Style::new().fg(TEXT),
        )]);
        frame.render_widget(
            Paragraph::new(head),
            Rect::new(inner.x, y, NAME_W, 1).intersection(inner),
        );
        frame.render_widget(
            Paragraph::new(metric(&n.link, |v| bits(*v as f64))),
            Rect::new(inner.x, y + 1, NAME_W, 1).intersection(inner),
        );
        let graph_w = inner.width.saturating_sub(NAME_W + RATE_W);
        for (k, (series, m, color, arrow)) in [
            (st.send.get_mut(&n.alias), &n.send, NET_SEND, "↑"),
            (st.recv.get_mut(&n.alias), &n.recv, NET_RECV, "↓"),
        ]
        .into_iter()
        .enumerate()
        {
            let row = y + k as u16 * half;
            if row >= bottom {
                continue;
            }
            let h = half.min(bottom - row);
            if let Some(s) = series
                && graph_w > 0
            {
                let scaled = s.scaled(usize::from(graph_w) * 2);
                frame.render_widget(
                    Graph {
                        samples: &scaled,
                        color: Some(color),
                    },
                    Rect::new(inner.x + NAME_W, row, graph_w, h).intersection(inner),
                );
            }
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(format!(" {arrow} "), Style::new().fg(color)),
                    metric(m, |v| bits(*v)),
                ])),
                Rect::new(inner.x + NAME_W + graph_w, row, RATE_W, 1).intersection(inner),
            );
        }
        y += half * 2;
    }
    if !down.is_empty() && y < bottom {
        let names: Vec<String> = down.iter().map(|n| printable(&n.alias)).collect();
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                label("disconnected: "),
                Span::styled(names.join(", "), Style::new().fg(NEUTRAL)),
            ])),
            Rect::new(inner.x, y, inner.width, 1).intersection(inner),
        );
    }
}

fn draw_game(frame: &mut Frame<'_>, area: Rect, st: &mut State) {
    let block = boxed("game", BORDER_GAME);
    let Some(fps) = &st.fps else {
        waiting(frame, area, block);
        return;
    };
    let g = match &fps.game {
        Ok(g) => g,
        Err(e) => {
            let mut lines = vec![
                Line::from(metric::<()>(&Err(e.clone()), |()| String::new())),
                Line::from(vec![label("RTSS "), metric(&fps.rtss, Clone::clone)]),
            ];
            if let Err(Missing::Fail(_)) = &fps.pipe {
                lines.push(Line::from(vec![
                    label("frametime pipe "),
                    metric(&fps.pipe, |_| String::new()),
                ]));
            }
            frame.render_widget(Paragraph::new(lines).block(block), area);
            return;
        }
    };
    let title = format!(
        "game  {} ({}){}",
        printable(&g.name),
        g.api,
        if g.approx {
            "  approx. (game opened before wintop)"
        } else {
            ""
        }
    );
    let block = boxed(&title, BORDER_GAME);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [graph, info] = Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).areas(inner);
    let scaled = st.game_fps.scaled(usize::from(graph.width) * 2);
    frame.render_widget(
        Graph {
            samples: &scaled,
            color: Some(gradient(0.0)),
        },
        graph,
    );
    let ms = |v: f64| format!("{} ms", num(v, 2));
    let lines = vec![
        Line::from(vec![
            label("FPS "),
            metric(&g.fps, |v| num(*v, 0)).add_modifier(Modifier::BOLD),
            sep(),
            label("1% "),
            metric(&g.lows, |(a, _)| num(*a, 0)),
            sep(),
            label("0.1% "),
            metric(&g.lows, |(_, b)| num(*b, 0)),
            sep(),
            metric(&g.resolution, |(x, y)| format!("{x}x{y}")),
        ]),
        Line::from(vec![
            label("frametime "),
            metric(&g.frametime, |(a, lo, hi)| {
                format!("{} ({} - {})", ms(*a), num(*lo, 1), num(*hi, 1))
            }),
        ]),
        Line::from(vec![
            label("GPU busy "),
            metric(&g.gpu_time, |(a, _)| ms(*a)),
            sep(),
            label("limited by "),
            metric(&g.bound, ToString::to_string),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines), info);
}

fn draw_footer(frame: &mut Frame<'_>, area: Rect, st: &State) {
    let mut spans = vec![
        Span::styled(" + - ", Style::new().fg(TITLE).add_modifier(Modifier::BOLD)),
        label(&format!("interval {} ms", st.interval_ms)),
        Span::styled("   q ", Style::new().fg(TITLE).add_modifier(Modifier::BOLD)),
        label("quit"),
    ];
    if let Some(f) = &st.fast {
        if let Ok(o) = &f.own {
            spans.extend([
                label("   wintop "),
                metric(&o.cpu, |v| format!("{}%", num(*v, 2))),
                Span::raw(" "),
                metric(&o.private_ws, |v| bytes(*v)),
            ]);
        }
        spans.extend([
            label("   collection "),
            Span::styled(
                format!("{} ms", num(f.took.as_secs_f64() * 1e3, 1)),
                Style::new().fg(TEXT),
            ),
        ]);
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::new().fg(DIM)),
        area,
    );
}
