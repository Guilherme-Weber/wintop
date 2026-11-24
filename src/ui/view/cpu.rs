//! Caixa da CPU (gráfico total, grade por lógico, termômetro vertical) e memória (barra de RAM
//! segmentada com o jogo destacado, commit, pagefile e pools do kernel).

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::state::{Series, State};
use super::{
    TEMP_BAR_W, bar_line, bar_sep, boxed, brief, draw_temp_bar, empty_box, label, line, metric,
    metric_styled, minmax, missing, put, section_divider, sep, short_bytes, temp_colored, text,
    text_room, with_failure, wrap,
};
use crate::fmt::{bytes, num};
use crate::model::{Memory, Metric};
use crate::ui::theme::th;
use crate::ui::widgets::{Graph, GraphColor, Meter, Segmented};
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

/// Largura do gráfico total da CPU (~45%; a grade por lógico fica com o resto).
fn total_width(w: u16) -> u16 {
    (w * 9 / 20).max(16).min(w.saturating_sub(16))
}

/// Caixa da CPU com a memória nas últimas `mem_rows` linhas (depois de um divisor).
/// Devolve a largura do gráfico de cada célula por lógico (para a GPU usar a mesma largura nas
/// células das engines quando a grade está visível).
pub(super) fn draw_cpu(
    frame: &mut Frame<'_>,
    area: Rect,
    st: &mut State,
    mem_rows: u16,
) -> Option<u16> {
    let Some(Ok(c)) = st.fast.as_ref().map(|f| &f.cpu) else {
        // CPU sem dado: o estado dela em cima, a memória continua embaixo.
        let [top, bottom] =
            Layout::vertical([Constraint::Min(3), Constraint::Length(mem_rows + 2)]).areas(area);
        empty_box(frame, top, "cpu", st.fast.as_ref().map(|f| &f.cpu));
        let block = boxed("memory", "");
        let inner = block.inner(bottom);
        frame.render_widget(block, bottom);
        draw_memory_body(frame, inner, st);
        return None;
    };
    let name = c
        .name
        .as_ref()
        .map_or_else(|_| "cpu".to_string(), |n| format!("cpu · {n}"));
    let per_len = c.per_logical.as_ref().map_or(0, Vec::len);
    let sensors = st.fast.as_ref().map(|f| &f.cpu_sensors);
    let cpu_tctl = sensors
        .and_then(|r| r.as_ref().ok())
        .and_then(|s| s.tctl.as_ref().ok())
        .copied();
    let inner_w = area.width.saturating_sub(2);
    let graphs_w = if cpu_tctl.is_some() && inner_w >= 44 {
        inner_w.saturating_sub(TEMP_BAR_W)
    } else {
        inner_w
    };
    // Janela real do gráfico total: 2 amostras por coluna, no intervalo atual.
    let cols = u64::from(total_width(graphs_w).saturating_sub(1));
    let secs = cols * 2 * st.interval_ms / 1000;
    let block = with_failure(
        boxed(&name, &format!("total {secs}s · {per_len} logical")),
        c.name.as_ref().err(),
    );
    let whole = block.inner(area);
    frame.render_widget(block, area);
    // Memória embaixo, depois de um divisor rotulado (como a VRAM na caixa da GPU).
    let [inner, divider, mem] = Layout::vertical([
        Constraint::Min(4),
        Constraint::Length(1),
        Constraint::Length(mem_rows),
    ])
    .areas(whole);
    section_divider(frame, divider, whole, "memory");
    draw_memory_body(frame, mem, st);
    // Linhas de sensores e topologia.
    // Clock: média de todos os lógicos e o pico (o lógico mais rápido agora).
    let mut l1 = vec![
        label("speed avg "),
        metric_styled(&c.freq_mhz, th().accent, |v| {
            format!("{} GHz", num(v / 1000.0, 2))
        }),
        label("  peak "),
        metric_styled(&c.freq_per_logical, th().accent, |f| {
            let peak = f.iter().copied().fold(0.0_f64, f64::max);
            format!("{} GHz", num(peak / 1000.0, 2))
        }),
        Span::raw(" "),
        metric(&c.base_mhz, |m| {
            format!("(base {})", num(f64::from(*m) / 1000.0, 2))
        }),
    ];
    // Sem o tempo de kernel por lógico a grade mostra só o total: a causa aparece.
    if let Err(e) = &c.kernel_per_logical {
        l1.extend([sep(), label("kernel per core "), missing(e)]);
    }
    match sensors {
        Some(Ok(s)) => {
            l1.extend([sep(), label("Tctl "), temp_colored(&s.tctl, 95.0)]);
            if let Some((_, hi)) = st.session.tctl.get() {
                l1.push(label(&format!(" (max {})", num(hi, 1))));
            }
            match &s.ccds {
                Ok(list) => {
                    for (n, t) in list {
                        l1.extend([
                            sep(),
                            label(&format!("CCD{n} ")),
                            temp_colored(&Ok(*t), 95.0),
                        ]);
                    }
                }
                Err(e) => l1.extend([sep(), label("CCD "), missing(e)]),
            }
            l1.extend([
                sep(),
                label("power "),
                metric_styled(&s.package_power, th().accent, |v| {
                    format!("{} W", num(*v, 1))
                }),
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
        metric_styled(
            &c.virtualization,
            if c.virtualization == Ok(true) {
                th().ok
            } else {
                th().neutral
            },
            |v| {
                if *v {
                    "enabled".into()
                } else {
                    "disabled".into()
                }
            },
        ),
        bar_sep(),
        metric(&c.counts, |n| {
            format!(
                "{} proc  {} thr  {} handles",
                n.processes, n.threads, n.handles
            )
        }),
    ]);
    // Uso e kernel no cabeçalho do gráfico, que é estreito: as causas vêm aqui, inteiras.
    let mut causes = Vec::new();
    for (name, m) in [("usage ", &c.usage), ("kernel ", &c.kernel)] {
        if let Err(e) = m {
            causes.push(vec![label(name), missing(e)]);
        }
    }
    let info_lines: Vec<Line<'static>> = causes
        .into_iter()
        .chain([l1, l2])
        .flat_map(|l| wrap(l, inner.width, 0, 2))
        .collect();
    let [graphs, info] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(info_lines.len() as u16),
    ])
    .areas(inner);
    put(frame, Paragraph::new(info_lines), info, info);
    // Termômetro vertical à direita quando há espaço e leitura de Tctl.
    let show_temp = cpu_tctl.is_some() && graphs.width >= 44 && graphs.height >= 4;
    let (graphs_main, temp_area) = if show_temp {
        let [left, t] =
            Layout::horizontal([Constraint::Min(39), Constraint::Length(TEMP_BAR_W)]).areas(graphs);
        (left, Some(t))
    } else {
        (graphs, None)
    };
    if let (Some(t_val), Some(t_area)) = (cpu_tctl, temp_area) {
        draw_temp_bar(frame, t_area, t_val, 95.0);
    }
    // Total à esquerda (~45%), grade por lógico no resto.
    let total_w = total_width(graphs_main.width);
    let [total, grid] =
        Layout::horizontal([Constraint::Length(total_w), Constraint::Min(8)]).areas(graphs_main);

    // Gráfico total, com o valor e os extremos na 1ª linha.
    let head = vec![
        brief(&c.usage, |v| format!("{}%", num(*v, 0))).add_modifier(Modifier::BOLD),
        Span::raw(" "),
        label("kern "),
        brief(&c.kernel, |v| format!("{}%", num(*v, 0))),
    ];
    line(frame, head, total.x, total.y, total.width, total);
    let mm = minmax(st.session.cpu, |v| format!("{}%", num(v, 0)));
    line(frame, vec![mm], total.x, total.y + 1, total.width, total);
    let g = Rect::new(
        total.x,
        total.y + 2,
        total.width.saturating_sub(1),
        total.height.saturating_sub(2),
    );
    frame.render_widget(
        Graph::new(st.cpu.values(), GraphColor::Height).with_under(st.cpu_kernel.values()),
        g.intersection(total),
    );

    match &c.per_logical {
        Ok(per) => draw_core_grid(
            frame,
            grid,
            &mut st.cores,
            &mut st.cores_kernel,
            per,
            c.freq_per_logical.as_deref().ok(),
        ),
        Err(e) => {
            put(
                frame,
                Paragraph::new(wrap(vec![label("per core "), missing(e)], grid.width, 0, 2)),
                grid,
                grid,
            );
            None
        }
    }
}

/// Grade de gráficos por lógico (4×4 com 16 threads), sempre com todos: se a célula ficar
/// pequena demais para gráfico, vira uma lista compacta de percentuais. Devolve a largura do
/// gráfico de uma célula quando desenhada como grade.
fn draw_core_grid(
    frame: &mut Frame<'_>,
    area: Rect,
    cores: &mut [Series],
    kernel: &mut [Series],
    per: &[f64],
    freq: Option<&[f64]>,
) -> Option<u16> {
    // Clock de cada lógico ao lado do uso ("C0  90% 4.05 GHz"), se couber.
    let ghz = |i: usize| freq.and_then(|f| f.get(i)).map(|mhz| num(mhz / 1000.0, 2));
    let n = per.len();
    if n == 0 || area.width == 0 || area.height == 0 {
        return None;
    }
    let cols = grid_cols(n);
    let rows = n.div_ceil(cols);
    let cw = area.width / cols as u16;
    let ch = area.height / rows as u16;
    if cw < 9 || ch < 2 {
        // Lista compacta: todos os lógicos, quebrando linha só entre um lógico e outro (cada
        // grupo começa com espaço; uso e clock ficam no mesmo trecho, que nunca é partido).
        // Todos os lógicos sempre aparecem: sem altura para os clocks, só o uso (média e pico
        // continuam na linha de velocidade).
        let list = |clocks: bool, gap: &str, k: usize| {
            let mut spans: Vec<Span<'static>> = per
                .iter()
                .take(k)
                .enumerate()
                .flat_map(|(i, v)| {
                    // Sem preenchimento à esquerda: um espaço no começo abriria outro grupo.
                    let pct = format!("{}%", num(*v, 0));
                    let g = if clocks { ghz(i) } else { None };
                    [
                        label(&format!("{gap}C{i} ")),
                        text(g.map_or_else(|| pct.clone(), |g| format!("{pct} {g}"))),
                    ]
                })
                .collect();
            if k < per.len() {
                spans.push(label(&format!(" +{} more (z)", per.len() - k)));
            }
            wrap(spans, area.width, 0, 0)
        };
        let n = per.len();
        let fits = |l: &Vec<Line<'static>>| l.len() <= usize::from(area.height);
        let mut lines = list(true, "  ", n);
        if !fits(&lines) {
            lines = list(false, "  ", n);
        }
        if !fits(&lines) {
            lines = list(false, " ", n);
        }
        // Nem assim: os que couberem e quantos ficaram de fora (a tecla z amplia a caixa).
        let mut k = n;
        while !fits(&lines) && k > 0 {
            k -= 1;
            lines = list(false, " ", k);
        }
        put(frame, Paragraph::new(lines), area, area);
        return None;
    }
    // A sobra da divisão (linhas e colunas) vai para as primeiras fileiras e colunas: a grade
    // ocupa a área inteira, sem faixa vazia embaixo.
    let (extra_h, extra_w) = (area.height % rows as u16, area.width % cols as u16);
    let at = |k: u16, size: u16, extra: u16| k * size + k.min(extra);
    let narrowest = cw.saturating_sub(1);
    for (i, v) in per.iter().enumerate() {
        let (c, r) = ((i % cols) as u16, (i / cols) as u16);
        let (x, y) = (at(c, cw, extra_w), at(r, ch, extra_h));
        let (w, h) = (cw + u16::from(c < extra_w), ch + u16::from(r < extra_h));
        let cell = Rect::new(area.x + x, area.y + y, w.saturating_sub(1), h).intersection(area);
        // Índice e percentual sempre separados ("C12 100%" = 8 colunas, nunca "C12100%").
        let mut head = vec![
            label(&format!("C{i:<2} ")),
            text(format!("{:>4}", format!("{}%", num(*v, 0)))),
        ];
        // Clock na mesma linha do uso: "C12 100% 4.05 GHz" com 17 colunas, "4.05G" com 14, só
        // o número com 13; sem largura, numa linha própria acima do gráfico (se houver altura).
        // O formato vem da célula mais estreita: a grade inteira fica igual.
        let mut graph_top = 1;
        if let Some(g) = ghz(i) {
            if narrowest >= 17 {
                head.push(label(&format!(" {g} GHz")));
            } else if narrowest >= 14 {
                head.push(label(&format!(" {g}G")));
            } else if narrowest >= 13 {
                head.push(label(&format!(" {g}")));
            } else if cell.height >= 3 {
                line(
                    frame,
                    vec![label(&format!("{g} GHz"))],
                    cell.x,
                    cell.y + 1,
                    cell.width,
                    cell,
                );
                graph_top = 2;
            }
        }
        line(frame, head, cell.x, cell.y, cell.width, cell);
        if let Some(s) = cores.get_mut(i) {
            let g = Rect::new(
                cell.x,
                cell.y + graph_top,
                cell.width,
                cell.height.saturating_sub(graph_top),
            );
            let mut graph = Graph::new(s.values(), GraphColor::Height);
            if let Some(k) = kernel.get_mut(i) {
                graph = graph.with_under(k.values());
            }
            frame.render_widget(graph, g.intersection(cell));
        }
    }
    Some(narrowest)
}

// --- Memória ---

/// Linhas de texto da caixa de memória, já quebradas na largura.
pub(super) struct MemText {
    legend: Vec<Line<'static>>,
    session: Vec<Line<'static>>,
    pools: Vec<Line<'static>>,
}

impl MemText {
    /// Altura da caixa: RAM, legenda, commit, sessão, pagefile, pools e as bordas.
    pub(super) fn height(&self) -> u16 {
        (3 + self.legend.len() + self.session.len() + self.pools.len() + 2) as u16
    }
}

const RAM_LABEL_W: u16 = 9;

/// RAM privada (Private Working Set) do jogo em foco, quando a thread lenta leu o mesmo PID.
fn game_ram_bytes(st: &State) -> Option<u64> {
    let game_pid = st
        .fps
        .as_ref()
        .and_then(|f| f.game.as_ref().ok())
        .map(|g| g.pid)?;
    let (pid, bytes) = st.slow.as_ref()?.game_ram?;
    (pid == game_pid && bytes > 0).then_some(bytes)
}

/// Partes da RAM como o Task Manager: (jogo, demais em uso, modificada, standby, livre, total).
fn ram_parts(m: &Memory, game_ram: Option<u64>) -> Option<(u64, u64, u64, u64, u64, u64)> {
    let (used, total, sb, md) = (
        *m.used.as_ref().ok()?,
        *m.total.as_ref().ok()?,
        *m.standby.as_ref().ok()?,
        *m.modified.as_ref().ok()?,
    );
    // Em uso = usado - modificada; o jogo sai do "in use" (como sai do "app" na VRAM).
    let active = used.saturating_sub(md);
    let game = game_ram.unwrap_or(0).min(active);
    let in_use = active.saturating_sub(game);
    let free = total.saturating_sub(used).saturating_sub(sb);
    Some((game, in_use, md, sb, free, total))
}

pub(super) fn mem_text(st: &State, width: u16) -> Option<MemText> {
    let m = st.fast.as_ref()?.memory.as_ref().ok()?;
    let inner = width.saturating_sub(2);
    let legend = match ram_parts(m, game_ram_bytes(st)) {
        Some((game, in_use, md, sb, free, _)) => {
            let mut items = Vec::with_capacity(5);
            if game > 0 {
                items.push(("game", game, th().game));
            }
            items.extend([
                ("in use", in_use, th().ram_in_use),
                ("modified", md, th().ram_modified),
                ("standby", sb, th().ram_standby),
                ("free", free, th().dim),
            ]);
            let spans: Vec<Span<'static>> = items
                .into_iter()
                .flat_map(|(name, v, c)| {
                    [
                        Span::styled("  ■ ", Style::new().fg(c)),
                        label(&format!("{name} ")),
                        text(short_bytes(v)),
                    ]
                })
                .collect();
            wrap(spans, inner, RAM_LABEL_W, RAM_LABEL_W)
        }
        None => match m.standby.as_ref().err().or(m.modified.as_ref().err()) {
            Some(e) => wrap(
                vec![label("cache "), missing(e)],
                inner,
                RAM_LABEL_W,
                RAM_LABEL_W,
            ),
            None => wrap(
                vec![
                    Span::styled("  ■ ", Style::new().fg(th().ram_modified)),
                    label("modified "),
                    metric(&m.modified, |v| short_bytes(*v)),
                    Span::styled("  ■ ", Style::new().fg(th().ram_standby)),
                    label("standby "),
                    metric(&m.standby, |v| short_bytes(*v)),
                ],
                inner,
                RAM_LABEL_W,
                RAM_LABEL_W,
            ),
        },
    };
    let gb = |v: f64| bytes(v as u64);
    let session = wrap(
        vec![
            label("session RAM "),
            minmax(st.session.ram, gb),
            label("   commit "),
            minmax(st.session.commit, gb),
        ],
        inner,
        0,
        8,
    );
    let pools = wrap(
        vec![
            label("kernel pools"),
            label("   paged "),
            metric(&m.paged_pool, |v| bytes(*v)),
            label("   non-paged "),
            metric(&m.nonpaged_pool, |v| bytes(*v)),
        ],
        inner,
        0,
        8,
    );
    Some(MemText {
        legend,
        session,
        pools,
    })
}

/// Memória no rodapé da caixa da CPU (como a VRAM na da GPU), em `inner` (sem moldura).
fn draw_memory_body(frame: &mut Frame<'_>, inner: Rect, st: &State) {
    let mem = st.fast.as_ref().map(|f| &f.memory);
    let (Some(Ok(m)), Some(t)) = (mem, mem_text(st, inner.width + 2)) else {
        // Sem a memória: o estado (espera ou a falha com a causa) no lugar dela.
        let msg = match mem {
            Some(Err(e)) => vec![label("memory "), missing(e)],
            _ => vec![label("memory: waiting for the first collection")],
        };
        put(
            frame,
            Paragraph::new(wrap(msg, inner.width, 0, 2)),
            inner,
            inner,
        );
        return;
    };
    let ram = m.used.clone().and_then(|u| Ok((u, m.total.clone()?)));
    ram_bar(frame, inner, m, game_ram_bytes(st), &ram);
    let mut y = 1u16;
    let lines = |frame: &mut Frame<'_>, ls: Vec<Line<'static>>, y: &mut u16| {
        for l in ls {
            put(
                frame,
                Paragraph::new(l),
                Rect::new(inner.x, inner.y + *y, inner.width, 1),
                inner,
            );
            *y += 1;
        }
    };
    lines(frame, t.legend, &mut y);
    bar_line(frame, inner, y, "commit  ", &m.commit);
    y += 1;
    lines(frame, t.session, &mut y);
    bar_line(frame, inner, y, "pagefile", &m.pagefile_usage);
    y += 1;
    lines(frame, t.pools, &mut y);
}

/// Barra da RAM (linha 0): cortada como o Task Manager (com o jogo destacado), ou só o usado sem
/// a divisão do cache.
fn ram_bar(
    frame: &mut Frame<'_>,
    area: Rect,
    m: &Memory,
    game_ram: Option<u64>,
    ram: &Metric<(u64, u64)>,
) {
    if area.height == 0 {
        return;
    }
    let t = metric(ram, |(u, t)| {
        format!(
            " {} / {} ({}%)",
            bytes(*u),
            bytes(*t),
            num(*u as f64 / (*t).max(1) as f64 * 100.0, 0)
        )
    });
    let label_w = RAM_LABEL_W.min(area.width);
    let text_w = text_room(&t, ram.is_err(), area.width, label_w);
    line(
        frame,
        vec![label("RAM      ")],
        area.x,
        area.y,
        label_w,
        area,
    );
    let bar_w = area.width.saturating_sub(label_w + text_w);
    let bar = Rect::new(area.x + label_w, area.y, bar_w, 1).intersection(area);
    match (ram_parts(m, game_ram), ram) {
        (Some((game, in_use, md, sb, _, total)), _) => {
            let mut parts = Vec::with_capacity(4);
            if game > 0 {
                parts.push((game, th().game));
            }
            parts.extend([
                (in_use, th().ram_in_use),
                (md, th().ram_modified),
                (sb, th().ram_standby),
            ]);
            frame.render_widget(
                Segmented {
                    parts: &parts,
                    total,
                },
                bar,
            );
        }
        (None, Ok((u, t))) => {
            let v = if *t > 0 { *u as f64 / *t as f64 } else { 0.0 };
            frame.render_widget(Meter { value: v }, bar);
        }
        (None, Err(_)) => {}
    }
    put(
        frame,
        Paragraph::new(t),
        Rect::new(area.x + label_w + bar_w, area.y, text_w, 1),
        area,
    );
}
