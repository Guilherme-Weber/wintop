//! Caixa da CPU (gráfico total, grade por lógico, termômetro vertical) e memória (barra de RAM
//! segmentada com o jogo destacado, commit, pagefile e pools do kernel).

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::state::{Series, State};
use super::{
    TEMP_BAR_W, aligned_bar_text_w, bar_line, bar_sep, bar_value, boxed, brief, draw_temp_bar,
    empty_box, faded, label, line, metric, metric_styled, minmax, missing, put,
    section_divider_right, sep, temp_colored, text, text_room, usage_color, v_divider,
    with_failure, wrap,
};
use crate::fmt::{bytes, num};
use crate::i18n::tr;
use crate::model::{Memory, Metric, Missing};
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

/// Caixa da CPU com a memória nas últimas linhas (depois de um divisor).
/// Devolve a largura do gráfico de cada célula por lógico (para a GPU usar a mesma largura nas
/// células das engines quando a grade está visível).
pub(super) fn draw_cpu(
    frame: &mut Frame<'_>,
    area: Rect,
    st: &State,
    mem_pre: Option<MemText>,
) -> Option<u16> {
    let mem_rows = mem_pre.as_ref().map_or(2, |t| t.height().saturating_sub(2));
    let Some(Ok(c)) = st.fast.as_ref().map(|f| &f.cpu) else {
        // CPU sem dado: o estado dela em cima, a memória continua embaixo.
        let [top, bottom] =
            Layout::vertical([Constraint::Min(3), Constraint::Length(mem_rows + 2)]).areas(area);
        empty_box(frame, top, "cpu", st.fast.as_ref().map(|f| &f.cpu));
        let block = boxed("memory", "");
        let inner = block.inner(bottom);
        frame.render_widget(block, bottom);
        draw_memory_body(frame, inner, st, mem_pre);
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
    let right = match &c.topology {
        Ok(t) => format!("{} cores · {} threads", t.cores, t.logical),
        Err(_) => format!("{per_len} threads"),
    };
    let block = with_failure(boxed(&name, &right), c.name.as_ref().err());
    let whole = block.inner(area);
    frame.render_widget(block, area);
    // Memória embaixo, depois de um divisor rotulado (como a VRAM na caixa da GPU).
    let [inner, divider, mem] = Layout::vertical([
        Constraint::Min(4),
        Constraint::Length(1),
        Constraint::Length(mem_rows),
    ])
    .areas(whole);
    let mem_hw = st
        .fast
        .as_ref()
        .and_then(|f| f.memory.as_ref().ok())
        .map_or_default(|m| memory_divider_right(m, divider.width));
    section_divider_right(frame, divider, whole, "memory", &mem_hw);
    draw_memory_body(frame, mem, st, mem_pre);
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
        metric_styled(&c.base_mhz, th().label, |m| {
            format!("(base {} GHz)", num(f64::from(*m) / 1000.0, 2))
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
    let mut l2 = match &c.topology {
        Ok(t) => vec![
            text(format!("{}S · {}C/{}T", t.sockets, t.cores, t.logical)),
            label("  L1 "),
            text(bytes(t.l1)),
            label("  L2 "),
            text(bytes(t.l2)),
            label("  L3 "),
            text(bytes(t.l3)),
        ],
        Err(e) => vec![missing(e)],
    };
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
    ]);
    match &c.counts {
        Ok(n) => l2.extend([
            text(format!("{} ", n.processes)),
            label("proc"),
            sep(),
            text(format!("{} ", n.threads)),
            label("thr"),
            sep(),
            text(format!("{} ", n.handles)),
            label("handles"),
        ]),
        Err(e) => l2.push(missing(e)),
    }
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
    // Total à esquerda (~45%), divisor vertical fino e grade por lógico no resto.
    let total_w = total_width(graphs_main.width);
    let [total, sep_col, grid] = Layout::horizontal([
        Constraint::Length(total_w.saturating_sub(1)),
        Constraint::Length(2),
        Constraint::Min(8),
    ])
    .areas(graphs_main);
    v_divider(frame, sep_col.x, sep_col.y, sep_col.height, graphs_main);

    // Gráfico total: valor colorido pela carga + kernel + min/max na mesma linha quando couber
    // (libera +1 linha inteira para o gráfico).
    let usage_span = match &c.usage {
        Ok(v) => Span::styled(
            format!("{}%", num(*v, 0)),
            Style::new()
                .fg(usage_color(*v))
                .add_modifier(Modifier::BOLD),
        ),
        _ => brief(&c.usage, |v| format!("{}%", num(*v, 0))).add_modifier(Modifier::BOLD),
    };
    let mut head = vec![
        usage_span,
        Span::raw(" "),
        label("kern "),
        brief(&c.kernel, |v| format!("{}%", num(*v, 0))),
    ];
    let mm = minmax(st.session.cpu, |v| format!("{}%", num(v, 0)));
    let head_w: usize = head.iter().map(Span::width).sum();
    let mm_w = mm.width();
    let graph_top = if mm_w == 0 {
        1
    } else if head_w + 2 + mm_w <= usize::from(total.width) {
        head.extend([sep(), mm]);
        1
    } else if head_w + 1 + mm_w <= usize::from(total.width) {
        head.extend([Span::raw(" "), mm]);
        1
    } else if total.height >= 3 {
        line(frame, vec![mm], total.x, total.y + 1, total.width, total);
        2
    } else {
        1
    };
    line(frame, head, total.x, total.y, total.width, total);
    let g = Rect::new(
        total.x,
        total.y + graph_top,
        total.width.saturating_sub(1),
        total.height.saturating_sub(graph_top),
    );
    frame.render_widget(
        Graph::new(st.cpu.values(), GraphColor::Height)
            .with_under(st.cpu_kernel.values())
            .with_baseline(th().dim),
        g.intersection(total),
    );

    match &c.per_logical {
        Ok(per) => draw_core_grid(
            frame,
            grid,
            &st.cores,
            &st.cores_kernel,
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
    cores: &[Series],
    kernel: &[Series],
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
                        text(g.map_or_else(|| pct.clone(), |g| format!("{pct} {g} GHz"))),
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
        let cw_us = usize::from(cell.width);
        let id = format!("C{i}");
        let pct_str = format!("{}%", num(*v, 0));
        let pct_span = faded(text(pct_str.clone()), *v < 0.5);
        // Clock sempre com unidade completa (`GHz`): na mesma linha do uso com 17+ colunas
        // ("C12 100% 4.05 GHz"); abaixo disso, na 2ª linha da célula (quando houver altura).
        // O formato vem da célula mais estreita: a grade inteira fica igual.
        let mut graph_top = 1;
        let inline_clk = ghz(i).and_then(|g| {
            if narrowest >= 17 {
                Some(format!(" {g} GHz"))
            } else {
                if cell.height >= 3 {
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
                None
            }
        });
        let right_w = pct_str.len() + inline_clk.as_ref().map_or(0, String::len);
        let pad = cw_us.saturating_sub(id.len() + right_w).max(1);
        let mut head = vec![label(&format!("{id}{:pad$}", "")), pct_span];
        if let Some(clk) = inline_clk {
            head.push(label(&clk));
        }
        line(frame, head, cell.x, cell.y, cell.width, cell);
        if let Some(s) = cores.get(i) {
            let g = Rect::new(
                cell.x,
                cell.y + graph_top,
                cell.width,
                cell.height.saturating_sub(graph_top),
            );
            let mut graph = Graph::new(s.values(), GraphColor::Height).with_baseline(th().dim);
            if let Some(k) = kernel.get(i) {
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
    alerts: Vec<Line<'static>>,
}

impl MemText {
    /// Altura da caixa: RAM, legenda, commit, sessão, pagefile, pools, alertas e as bordas.
    pub(super) fn height(&self) -> u16 {
        (3 + self.legend.len() + self.session.len() + self.pools.len() + self.alerts.len() + 2)
            as u16
    }
}

const RAM_LABEL_W: u16 = 9;

/// Resumo de hardware de RAM para o lado direito do divisor `── memory ──`.
fn memory_divider_right(m: &Memory, width: u16) -> String {
    let mut base = Vec::new();
    if let Ok(hw) = &m.hardware {
        base.push(hw.kind.to_string());
        if let Some(spd) = hw.speed_mt {
            base.push(format!("{spd} MT/s"));
        }
        let slots = match hw.form_factor {
            Some(ff) => format!("{}/{} {ff}", hw.slots_used, hw.slots_total),
            None => format!("{}/{}", hw.slots_used, hw.slots_total),
        };
        base.push(slots);
    }
    let summary = base.join(" · ");
    // Reserva espaço para "── memory ────  ──" (16 colunas no mínimo).
    let room = usize::from(width.saturating_sub(18));
    if !summary.is_empty() && Span::raw(summary.as_str()).width() <= room {
        return summary;
    }
    String::new()
}

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

/// Partes da RAM como o Task Manager (com o jogo e a compressão separados do "in use").
struct RamParts {
    game: u64,
    in_use: u64,
    compressed: u64,
    uncompressed: u64,
    modified: u64,
    standby: u64,
    free: u64,
    total: u64,
}

fn ram_parts(m: &Memory, game_ram: Option<u64>) -> Option<RamParts> {
    let (used, total, standby, modified) = (
        *m.used.as_ref().ok()?,
        *m.total.as_ref().ok()?,
        *m.standby.as_ref().ok()?,
        *m.modified.as_ref().ok()?,
    );
    // Em uso = usado - modificada; o jogo e a memória comprimida saem do "in use".
    let active = used.saturating_sub(modified);
    let game = game_ram.unwrap_or(0).min(active);
    let rem = active.saturating_sub(game);
    let (compressed, uncompressed) = match &m.compressed {
        Ok(c) => (c.compressed.min(rem), c.uncompressed),
        Err(_) => (0, 0),
    };
    let in_use = rem.saturating_sub(compressed);
    let free = total.saturating_sub(used).saturating_sub(standby);
    Some(RamParts {
        game,
        in_use,
        compressed,
        uncompressed,
        modified,
        standby,
        free,
        total,
    })
}

pub(super) fn mem_text(st: &State, width: u16) -> Option<MemText> {
    let m = st.fast.as_ref()?.memory.as_ref().ok()?;
    let inner = width.saturating_sub(2);
    let legend = match ram_parts(m, game_ram_bytes(st)) {
        Some(p) => {
            let mut spans = Vec::with_capacity(20);
            let push_item =
                |spans: &mut Vec<Span<'static>>, name: &str, v: u64, c: ratatui::style::Color| {
                    spans.extend([
                        Span::styled("  ■ ", Style::new().fg(c)),
                        label(&format!("{name} ")),
                        text(bytes(v)),
                    ]);
                };
            if p.game > 0 {
                push_item(&mut spans, "game", p.game, th().game);
            }
            push_item(&mut spans, "in use", p.in_use, th().ram_in_use);
            if p.compressed > 0 {
                if p.uncompressed > p.compressed {
                    spans.extend([
                        Span::styled("  ■ ", Style::new().fg(th().ram_compressed)),
                        label("compressed "),
                        text(format!("{} ", bytes(p.compressed))),
                        Span::styled(
                            format!("({} {})", bytes(p.uncompressed), tr("raw")),
                            Style::new().fg(th().label),
                        ),
                    ]);
                } else {
                    push_item(&mut spans, "compressed", p.compressed, th().ram_compressed);
                }
            }
            push_item(&mut spans, "modified", p.modified, th().ram_modified);
            push_item(&mut spans, "standby", p.standby, th().ram_standby);
            push_item(&mut spans, "free", p.free, th().neutral);
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
                    metric(&m.modified, |v| bytes(*v)),
                    Span::styled("  ■ ", Style::new().fg(th().ram_standby)),
                    label("standby "),
                    metric(&m.standby, |v| bytes(*v)),
                ],
                inner,
                RAM_LABEL_W,
                RAM_LABEL_W,
            ),
        },
    };
    let gb = |v: f64| bytes(v as u64);
    let mut s_spans = vec![
        label("session RAM "),
        minmax(st.session.ram, gb),
        label("   commit "),
        minmax(st.session.commit, gb),
    ];
    match &m.commit_peak {
        Ok(pk) if *pk > 0 => {
            s_spans.extend([label("  boot peak "), text(bytes(*pk))]);
        }
        Ok(_) => {}
        Err(e) => s_spans.extend([label("  boot peak "), missing(e)]),
    }
    let session = wrap(s_spans, inner, 0, 8);

    let mut p_spans = vec![
        label("kernel pools"),
        label("   paged "),
        metric(&m.paged_pool, |v| bytes(*v)),
        label("   non-paged "),
        metric(&m.nonpaged_pool, |v| bytes(*v)),
        bar_sep(),
        label("paging "),
    ];
    match &m.paging {
        Ok((pin, pout)) => {
            let rate_span = |v: f64| faded(text(format!("{}/s", num(v, 0))), v < 0.5);
            p_spans.extend([
                label("in "),
                rate_span(*pin),
                label("  out "),
                rate_span(*pout),
            ]);
        }
        Err(e) => p_spans.push(missing(e)),
    }
    let pools = wrap(p_spans, inner, 0, 8);

    // Alertas e falhas de fontes auxiliares da memória (zero linhas quando tudo está bem).
    let mut alerts = Vec::new();
    if let Ok(bad) = m.bad_pages
        && bad > 0
    {
        alerts.extend(wrap(
            vec![Span::styled(
                format!(
                    "! {}: {bad} ({})",
                    tr("bad RAM pages"),
                    tr("hardware fault")
                ),
                Style::new().fg(th().fail).add_modifier(Modifier::BOLD),
            )],
            inner,
            0,
            2,
        ));
    }
    for (name, err) in [
        ("compressed ", m.compressed.as_ref().err()),
        ("hardware ", m.hardware.as_ref().err()),
        ("hardware reserved ", m.hardware_reserved.as_ref().err()),
        ("bad pages ", m.bad_pages.as_ref().err()),
    ] {
        if let Some(e @ Missing::Fail(_)) = err {
            alerts.extend(wrap(vec![label(name), missing(e)], inner, 0, 2));
        }
    }

    Some(MemText {
        legend,
        session,
        pools,
        alerts,
    })
}

/// Memória no rodapé da caixa da CPU (como a VRAM na da GPU), em `inner` (sem moldura).
fn draw_memory_body(frame: &mut Frame<'_>, inner: Rect, st: &State, mem_pre: Option<MemText>) {
    let mem = st.fast.as_ref().map(|f| &f.memory);
    let (Some(Ok(m)), Some(t)) = (mem, mem_pre) else {
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
    let min_text_w = aligned_bar_text_w(&[&ram, &m.commit, &m.pagefile_usage]);
    ram_bar(frame, inner, m, game_ram_bytes(st), &ram, min_text_w);
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
    bar_line(frame, inner, y, "commit  ", &m.commit, min_text_w);
    y += 1;
    lines(frame, t.session, &mut y);
    bar_line(frame, inner, y, "pagefile", &m.pagefile_usage, min_text_w);
    y += 1;
    lines(frame, t.pools, &mut y);
    lines(frame, t.alerts, &mut y);
}

/// Barra da RAM (linha 0): cortada como o Task Manager (com jogo e compressão destacados), ou só
/// o usado sem a divisão do cache.
fn ram_bar(
    frame: &mut Frame<'_>,
    area: Rect,
    m: &Memory,
    game_ram: Option<u64>,
    ram: &Metric<(u64, u64)>,
    min_text_w: u16,
) {
    if area.height == 0 {
        return;
    }
    let (val_line, raw_w) = bar_value(ram);
    let label_w = RAM_LABEL_W.min(area.width);
    let target_w = if ram.is_err() {
        raw_w
    } else {
        raw_w.max(min_text_w)
    };
    let text_w = text_room(target_w, ram.is_err(), area.width, label_w);
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
        (Some(p), _) => {
            let mut parts = Vec::with_capacity(5);
            if p.game > 0 {
                parts.push((p.game, th().game));
            }
            parts.push((p.in_use, th().ram_in_use));
            if p.compressed > 0 {
                parts.push((p.compressed, th().ram_compressed));
            }
            parts.extend([
                (p.modified, th().ram_modified),
                (p.standby, th().ram_standby),
            ]);
            frame.render_widget(
                Segmented {
                    parts: &parts,
                    total: p.total,
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
    let p = if ram.is_err() {
        Paragraph::new(val_line)
    } else {
        Paragraph::new(val_line).right_aligned()
    };
    put(
        frame,
        p,
        Rect::new(area.x + label_w + bar_w, area.y, text_w, 1),
        area,
    );
}
