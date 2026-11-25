//! Caixa de rede compacta: placas ativas com link, taxas, máximo da sessão e gráfico espelhado.

use std::collections::HashMap;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::state::{MinMax, Series, State};
use super::{
    boxed, empty_box, faded, failed, label, line, metric_styled, missing, put, raw_text, wrap,
};
use crate::fmt::{bits, bytes};
use crate::model::{Missing, Nic};
use crate::ui::theme::th;
use crate::ui::widgets::{Graph, GraphColor};
use crate::win::printable;

/// Escala mínima da rede (100 Kbps): ruído de poucos bytes não parece tráfego.
const NET_FLOOR: f64 = 100_000.0;
// --- Rede ---

/// Taxa de rede na unidade do config (bits por segundo, como o Task Manager, ou bytes).
fn net_rate(st: &State, bits_per_s: f64) -> String {
    if st.net_bytes {
        format!("{}/s", bytes((bits_per_s / 8.0).max(0.0) as u64))
    } else {
        bits(bits_per_s)
    }
}

/// Rede compacta: por placa, 1 linha de identificação (nome, descrição, link, erros) e 1 de
/// taxas (↓/↑ com o máximo da sessão e a escala comum); o gráfico espelhado só entra com altura
/// sobrando (download em cima, upload embaixo). Placas desconectadas numa linha só.
pub(super) fn draw_net(frame: &mut Frame<'_>, area: Rect, st: &State) {
    let nics = st.fast.as_ref().map(|f| &f.nics);
    let Some(Ok(list)) = nics else {
        empty_box(frame, area, "net", nics);
        return;
    };
    let block = boxed("net", "↓ receive  ↑ send");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // Só o estado neutro ("disconnected") vai para a linha de desconectadas; falha aparece.
    let (down, up): (Vec<_>, Vec<_>) = list.iter().partition(|n| {
        matches!(n.link, Err(Missing::Neutral(_))) && !failed(&n.send) && !failed(&n.recv)
    });
    // Linhas fixas por placa: identificação + taxas (que podem quebrar em duas, estreita).
    let rate_lines = |n: &Nic| -> Vec<Line<'static>> {
        let mut spans = Vec::new();
        for (arrow, m, color, mm) in [
            ("↓ ", &n.recv, th().net_recv, st.session.recv.get(&n.alias)),
            (
                "  ↑ ",
                &n.send,
                th().net_send,
                st.session.send.get(&n.alias),
            ),
        ] {
            spans.push(Span::styled(arrow, Style::new().fg(color)));
            spans.push(faded(
                metric_styled(m, color, |v| net_rate(st, *v)),
                m.as_ref().is_ok_and(|v| *v < 1.0),
            ));
            if let Some((_, hi)) = mm.copied().and_then(MinMax::get) {
                spans.push(label(&format!(" max {}", net_rate(st, hi))));
            }
        }
        wrap(spans, inner.width, 0, 2)
    };
    let fixed: Vec<(usize, Vec<Line<'static>>)> = up.iter().map(|n| (1, rate_lines(n))).collect();
    let need = |k: usize| -> u16 {
        fixed
            .iter()
            .take(k)
            .map(|(h, r)| (*h + r.len()) as u16)
            .sum()
    };
    let reserved = u16::from(!down.is_empty());
    let room = inner.height.saturating_sub(reserved);
    let mut shown = up.len();
    while shown > 0 && need(shown) > room.saturating_sub(u16::from(shown < up.len())) {
        shown -= 1;
    }
    let more = shown < up.len();
    let bottom_rows = reserved.max(u16::from(more));
    let room = inner.height.saturating_sub(bottom_rows);
    // Sobra repartida entre as placas como gráfico espelhado (só a partir de 2 linhas).
    let spare = room.saturating_sub(need(shown));
    // Os gráficos ocupam toda a sobra (sem espaço vazio embaixo); com mais de uma placa, uma
    // linha em branco as separa.
    let gaps = shown.saturating_sub(1) as u16;
    let gap = gaps > 0 && spare >= gaps + 2 * shown as u16;
    let graph_h = if shown > 0 {
        spare.saturating_sub(if gap { gaps } else { 0 }) / shown as u16
    } else {
        0
    };
    let mut y = inner.y;
    for (k, (n, (_, rates))) in up.iter().zip(fixed).take(shown).enumerate() {
        if gap && k > 0 {
            y += 1;
        }
        // Identificação: nome e descrição à esquerda, erros e link à direita.
        let mut right = Vec::new();
        match &n.errors {
            Ok((0, 0)) => {}
            Ok((rx, tx)) => right.push(Span::styled(
                format!("errors ↓{rx} ↑{tx}  "),
                Style::new().fg(th().fail),
            )),
            Err(e) => right.extend([label("errors "), missing(e), Span::raw("  ")]),
        }
        right.extend([
            label("link "),
            metric_styled(&n.link, th().ok, |v| bits(*v as f64)),
        ]);
        let right = Line::from(right);
        let rw = (right.width() as u16).min(inner.width);
        line(
            frame,
            vec![
                raw_text(printable(&n.alias)).add_modifier(Modifier::BOLD),
                Span::styled(
                    format!("  {}", printable(&n.description)),
                    Style::new().fg(th().label),
                ),
            ],
            inner.x,
            y,
            inner.width.saturating_sub(rw + 1),
            inner,
        );
        put(
            frame,
            Paragraph::new(right).right_aligned(),
            Rect::new(inner.x, y, inner.width, 1),
            inner,
        );
        y += 1;
        for l in rates {
            put(
                frame,
                Paragraph::new(l),
                Rect::new(inner.x, y, inner.width, 1),
                inner,
            );
            y += 1;
        }
        if graph_h >= 2 {
            let n_samples = usize::from(inner.width) * 2;
            let peak =
                |m: &HashMap<String, Series>| m.get(&n.alias).map_or(0.0, |s| s.max_of(n_samples));
            let scale = peak(&st.send).max(peak(&st.recv)).max(NET_FLOOR);
            // Com ≥ 3 linhas, a escala ganha linha própria acima do gráfico (nunca sobrescreve
            // os picos mais recentes) e download/upload ficam com alturas exatamente simétricas.
            let (graph_y, half) = if graph_h >= 3 {
                put(
                    frame,
                    Paragraph::new(label(&format!("scale {}", net_rate(st, scale))))
                        .right_aligned(),
                    Rect::new(inner.x, y, inner.width, 1),
                    inner,
                );
                (y + 1, (graph_h - 1) / 2)
            } else {
                (y, graph_h / 2)
            };
            for (series, color, top, h, inverted) in [
                (st.recv.get(&n.alias), th().net_recv, graph_y, half, false),
                (
                    st.send.get(&n.alias),
                    th().net_send,
                    graph_y + half,
                    half,
                    true,
                ),
            ] {
                if let Some(s) = series {
                    // Linha de base apagada no meio: placa parada continua com o eixo visível.
                    let g = Graph::new(s.values(), GraphColor::Fixed(color))
                        .with_scale(scale)
                        .with_baseline(th().dim);
                    let g = if inverted { g.inverted() } else { g };
                    frame.render_widget(
                        g,
                        Rect::new(inner.x, top, inner.width, h).intersection(inner),
                    );
                }
            }
            y += graph_h;
        }
    }
    if !down.is_empty() || more {
        let names: Vec<String> = down.iter().map(|n| printable(&n.alias)).collect();
        let mut spans = Vec::new();
        if more {
            spans.push(label(&format!("+{} more  ", up.len() - shown)));
        }
        if !names.is_empty() {
            spans.extend([
                label("disconnected: "),
                Span::styled(names.join(", "), Style::new().fg(th().neutral)),
            ]);
        }
        line(
            frame,
            spans,
            inner.x,
            inner.y + inner.height.saturating_sub(1),
            inner.width,
            inner,
        );
    }
}
