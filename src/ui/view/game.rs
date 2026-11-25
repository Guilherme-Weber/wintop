//! Caixa do jogo: gráficos de FPS (com FPS real escurecido em frame gen) e frametime, lows de
//! 60 s, tempo de GPU e rolagem no ocioso até o histórico sair da tela.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::state::State;
use super::{
    bar_sep, boxed, failed_span, label, line, metric, metric_styled, minmax, missing, put, sep,
    v_divider, waiting, wrap,
};
use crate::fmt::num;
use crate::model::{FrameSource, Missing};
use crate::ui::theme::th;
use crate::ui::widgets::{Graph, GraphColor};
use crate::win::printable;

/// Teto do frametime (como o Afterburner); o limite de alerta vem do config.
const FT_CEIL_MS: f64 = 50.0;
/// Degraus do teto do gráfico de FPS (sobe para o próximo quando o máximo passa).
const FPS_STEPS: [f64; 8] = [60.0, 120.0, 165.0, 240.0, 360.0, 500.0, 750.0, 1000.0];
// --- Jogo ---

/// Cabeçalho de gráfico: o valor sempre; o mínimo e o máximo só se couberem inteiros (cortado,
/// "min 5." encostaria no rótulo do teto e viraria outro número).
fn head_fit(mut head: Vec<Span<'static>>, mm: Span<'static>, room: u16) -> Vec<Span<'static>> {
    let w: usize = head.iter().map(Span::width).sum::<usize>() + 2 + mm.width();
    if w <= usize::from(room) {
        head.extend([sep(), mm]);
    }
    head
}

/// Abaixo disto o gráfico de FPS muda de cor.
const FPS_LOW: f64 = 30.0;

/// Teto do gráfico de FPS: o menor degrau acima do máximo visível.
fn fps_ceiling(max: f64) -> f64 {
    FPS_STEPS
        .iter()
        .copied()
        .find(|s| *s >= max)
        .unwrap_or(FPS_STEPS[FPS_STEPS.len() - 1])
}

pub(super) fn draw_game(frame: &mut Frame<'_>, area: Rect, st: &State) {
    let Some(fps) = st.fps.as_deref() else {
        waiting(frame, area, boxed("game", ""));
        return;
    };
    let pipe_failed = matches!(fps.pipe, Err(Missing::Fail(_)));
    // Sem jogo em foco: "idle", e os gráficos continuam andando (com linha de base no ocioso).
    let g = fps.game.as_ref().ok();
    let rtss_failed = matches!(fps.rtss, Err(Missing::Fail(_)));
    // Só mostra escala e aviso do histórico enquanto ainda houver pontos finitos na janela visível.
    let vis_samples = usize::from(area.width.saturating_sub(2) / 2) * 2;
    let has_history =
        st.game_fps.has_finite_in(vis_samples) || st.game_ft.has_finite_in(vis_samples);
    let (title, source) = match g {
        Some(g) => {
            // Sem resolução: a causa vai para as linhas de baixo, com a cor do estado.
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
            let source = match (g.source, pipe_failed) {
                (FrameSource::Etw, _) => "ETW: exact",
                (FrameSource::Pipe, _) => "pipe: exact",
                (FrameSource::Approx, false) => "approx. (game opened before wintop)",
                (FrameSource::Approx, true) => "approx. (frametime pipe failed)",
            };
            (title, source)
        }
        None if rtss_failed => ("game · RTSS unavailable".to_string(), ""),
        None => ("game · idle".to_string(), ""),
    };
    let block = boxed(&title, source);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let mut lines = match (g, &fps.game) {
        (Some(g), _) => game_lines(g, st.interval_ms),
        (None, Err(_)) if rtss_failed => {
            let mut v = vec![Line::from(vec![
                label("RTSS "),
                metric(&fps.rtss, Clone::clone),
            ])];
            if has_history {
                v.push(Line::from(label("graphs scroll while idle · r resets")));
            }
            v
        }
        (None, Err(e)) => vec![
            Line::from(vec![missing(e)]),
            Line::from(vec![
                label("RTSS "),
                metric(&fps.rtss, Clone::clone),
                if has_history {
                    bar_sep()
                } else {
                    Span::raw("")
                },
                label(if has_history {
                    "graphs scroll while idle · r resets"
                } else {
                    ""
                }),
            ]),
        ],
        (None, Ok(_)) => Vec::new(),
    };
    // Falha do ETW (o FPS cai para o pipe do RTSS): linha própria, só quando acontece.
    if matches!(fps.etw, Err(Missing::Fail(_))) {
        lines.push(Line::from(vec![
            label("ETW frames "),
            metric(&fps.etw, |_| String::new()),
        ]));
    }
    // Falha do pipe (outro servidor ocupando o nome): linha própria, só quando acontece.
    if pipe_failed {
        lines.push(Line::from(vec![
            label("frametime pipe "),
            metric(&fps.pipe, |_| String::new()),
        ]));
    }
    let lines: Vec<Line<'static>> = lines
        .into_iter()
        .flat_map(|l| wrap(l.spans, inner.width, 0, 2))
        .collect();
    let info_h = lines.len() as u16;
    // Sem jogo: só o "idle" do título; os cabeçalhos ficam com "-".
    let idle = || label("-");
    let [graphs, info] =
        // As linhas de dados (FPS, lows, frametime...) têm prioridade sobre a altura dos gráficos.
        Layout::vertical([
            // Pelo menos o cabeçalho com o FPS e o frametime atuais.
            Constraint::Min(3.min(inner.height.saturating_sub(info_h)).max(1)),
            Constraint::Length(info_h),
        ])
        .areas(inner);
    put(frame, Paragraph::new(lines), info, info);
    let [fps_area, sep_col, ft_area] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(3),
        Constraint::Fill(1),
    ])
    .areas(graphs);
    v_divider(frame, sep_col.x + 1, sep_col.y, sep_col.height, graphs);

    // FPS: teto na taxa do monitor (ou em degraus pelo máximo visível).
    let n = usize::from(fps_area.width) * 2;
    // Teto = taxa do monitor principal (acima dela, cor própria); sem ela, degraus pelo máximo.
    let ceil = st
        .refresh_hz
        .map_or_else(|| fps_ceiling(st.game_fps.max_of(n)), f64::from);
    let ceil_label = if st.refresh_hz.is_some() {
        format!("{} Hz", num(ceil, 0))
    } else {
        format!("{} fps", num(ceil, 0))
    };
    let mut fps_head = vec![
        label("FPS "),
        // Sem valor: marca curta (a causa inteira está nas linhas de baixo).
        g.map_or_else(idle, |g| match &g.fps {
            Ok(v) => {
                let c = if *v < FPS_LOW {
                    th().fps_low
                } else if *v > ceil {
                    th().fps_over
                } else {
                    th().fps
                };
                Span::styled(num(*v, 0), Style::new().fg(c).add_modifier(Modifier::BOLD))
            }
            Err(Missing::Neutral(_)) => label("-"),
            Err(Missing::Fail(_)) => failed_span(),
        }),
    ];
    // Frame gen: "FPS 190 · real 47 (4x)" ou razão dinâmica "(2.5x)" (o real escuro, como no gráfico).
    if let Some(fg) = g.and_then(|g| g.framegen) {
        fps_head.push(label(" · real "));
        fps_head.push(
            Span::raw(num(fg.real_fps, 0))
                .style(Style::new().fg(crate::ui::widgets::darker(th().fps))),
        );
        if let Some(ratio) = fg.ratio_text() {
            fps_head.push(label(&format!(" ({ratio})")));
        }
    }
    let head = head_fit(
        fps_head,
        minmax(st.session.fps, |v| num(v, 0)),
        fps_area.width.saturating_sub(ceil_label.len() as u16 + 1),
    );
    line(
        frame,
        head,
        fps_area.x,
        fps_area.y,
        fps_area.width.saturating_sub(ceil_label.len() as u16 + 1),
        fps_area,
    );
    put(
        frame,
        Paragraph::new(label(if has_history { &ceil_label } else { "" })).right_aligned(),
        Rect::new(fps_area.x, fps_area.y, fps_area.width, 1),
        fps_area,
    );
    let gr = Rect::new(
        fps_area.x,
        fps_area.y + 1,
        fps_area.width,
        fps_area.height.saturating_sub(1),
    );
    let mut fps_graph = Graph::new(
        st.game_fps.values(),
        GraphColor::Bands {
            base: th().fps,
            over: th().fps_over,
            low: th().fps_low,
            low_at: FPS_LOW / ceil,
        },
    )
    .with_under(st.game_real.values())
    .with_scale(ceil);
    if has_history {
        fps_graph = fps_graph.with_baseline(th().dim);
    }
    frame.render_widget(fps_graph, gr.intersection(fps_area));

    // Frametime: teto fixo de 50 ms; picos acima do limite na cor de pico do tema.
    let head = head_fit(
        vec![
            label("FT "),
            // Como no FPS: marca curta sem valor (a causa inteira está nas linhas de baixo).
            g.map_or_else(idle, |g| match &g.frametime {
                Ok((a, _, _)) => {
                    let c = if *a >= st.frametime_alert_ms {
                        th().frametime_spike
                    } else {
                        th().frametime
                    };
                    Span::styled(
                        format!("{} ms", num(*a, 2)),
                        Style::new().fg(c).add_modifier(Modifier::BOLD),
                    )
                }
                Err(Missing::Neutral(_)) => label("-"),
                Err(Missing::Fail(_)) => failed_span(),
            }),
        ],
        minmax(st.session.ft, |v| format!("{} ms", num(v, 1))),
        ft_area.width.saturating_sub(6),
    );
    line(
        frame,
        head,
        ft_area.x,
        ft_area.y,
        ft_area.width.saturating_sub(6),
        ft_area,
    );
    put(
        frame,
        Paragraph::new(label(if has_history { "50 ms" } else { "" })).right_aligned(),
        Rect::new(ft_area.x, ft_area.y, ft_area.width, 1),
        ft_area,
    );
    let gr = Rect::new(
        ft_area.x,
        ft_area.y + 1,
        ft_area.width,
        ft_area.height.saturating_sub(1),
    );
    let mut ft_graph = Graph::new(
        st.game_ft.values(),
        GraphColor::Threshold {
            base: th().frametime,
            alert: th().frametime_spike,
            at: st.frametime_alert_ms / FT_CEIL_MS,
        },
    )
    .with_scale(FT_CEIL_MS);
    if has_history {
        ft_graph = ft_graph.with_baseline(th().dim);
    }
    frame.render_widget(ft_graph, gr.intersection(ft_area));
}

/// As duas linhas de detalhe do jogo em foco.
fn game_lines(g: &crate::model::Game, interval_ms: u64) -> Vec<Line<'static>> {
    let ms = |v: f64| format!("{} ms", num(v, 2));
    // O que não cabe no cabeçalho dos gráficos (estado do FPS) e no título (resolução).
    let mut extra = Vec::new();
    if let Err(e) = &g.fps {
        extra.push(Line::from(vec![label("FPS "), missing(e)]));
    }
    if let Err(e) = &g.resolution {
        extra.push(Line::from(vec![label("resolution "), missing(e)]));
    }
    let bound_color = match g.bound {
        Ok("GPU") => th().warn,
        Ok("CPU") => th().accent,
        _ => th().text,
    };
    let low1_color =
        g.lows.as_ref().map_or(
            th().fps,
            |(a, _)| if *a < FPS_LOW { th().fps_low } else { th().fps },
        );
    let low01_color =
        g.lows.as_ref().map_or(
            th().fps,
            |(_, b)| if *b < FPS_LOW { th().fps_low } else { th().fps },
        );
    extra
        .into_iter()
        .chain(vec![
            Line::from(vec![
                label("1% low "),
                metric_styled(&g.lows, low1_color, |(a, _)| num(*a, 0)),
                label(&format!("   {}% low ", num(0.1, 1))),
                metric_styled(&g.lows, low01_color, |(_, b)| num(*b, 0)),
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
                metric_styled(&g.bound, bound_color, ToString::to_string),
                bar_sep(),
                label("lows over 60 s"),
                bar_sep(),
                label(&format!("graphs synced to the {interval_ms} ms tick")),
            ]),
        ])
        .collect()
}

#[cfg(test)]
mod game_graph_tests {
    use super::*;

    /// Jogo em foco: 1 ponto por tick da UI mesmo sem publicação nova do RTSS (intervalo de
    /// 100 ms, jogo congelado), e o frametime sem frame no tick é lacuna, nunca 0 ms. No ocioso
    /// com histórico, empurra lacuna (NaN) para os gráficos continuarem andando.
    #[test]
    fn graficos_do_jogo_andam_no_ritmo_da_ui() {
        let mut st = State::new(100);
        let mut fps = crate::ui::tests::game();
        st.push_fps(fps.clone());
        st.push_fast(crate::ui::tests::fast(0));
        // Dois ticks sem publicação: o FPS atual se repete e o frametime fica sem valor.
        st.push_fast(crate::ui::tests::fast(1));
        st.push_fast(crate::ui::tests::fast(2));
        let (f, t): (Vec<f64>, Vec<f64>) =
            (st.game_fps.values().to_vec(), st.game_ft.values().to_vec());
        assert_eq!(f, [143.6, 143.6, 143.6]);
        assert!(
            (t[0] - 11.4).abs() < 1e-9 && t[1].is_nan() && t[2].is_nan(),
            "{t:?}"
        );
        // Congelado (sem frames no último segundo): FPS 0, ainda 1 ponto por tick.
        if let Ok(g) = &mut fps.game {
            g.fps = crate::model::neutral("no frames in the last second");
            g.peak_ms = None;
        }
        st.push_fps(fps.clone());
        st.push_fast(crate::ui::tests::fast(3));
        assert_eq!(st.game_fps.values().last(), Some(&0.0));
        assert_eq!(st.game_ft.values().len(), 4);
        // FPS real (frame gen) alinhado com o FPS: lacuna sem frame gen, valor com ele.
        if let Ok(g) = &mut fps.game {
            g.fps = Ok(190.0);
            g.framegen = Some(crate::model::FrameGen {
                real_fps: 47.0,
                multiplier: Some(4),
                effective: Some(4.0),
            });
        }
        st.push_fps(fps);
        st.push_fast(crate::ui::tests::fast(4));
        let real = st.game_real.values().to_vec();
        assert_eq!(real.len(), st.game_fps.values().len());
        assert!(real[..4].iter().all(|v| v.is_nan()), "{real:?}");
        assert_eq!(real[4], 47.0);
        // Ocioso (sem jogo em foco): continua avançando com NaN para o histórico sair pela esquerda.
        let mut idle = crate::ui::tests::game();
        idle.game = crate::model::neutral("no game in focus");
        st.push_fps(idle);
        st.push_fast(crate::ui::tests::fast(5));
        st.push_fast(crate::ui::tests::fast(6));
        assert_eq!(st.game_fps.values().len(), 7);
        assert!(st.game_fps.values()[5].is_nan() && st.game_fps.values()[6].is_nan());
        assert!(st.game_real.values()[5].is_nan() && st.game_ft.values()[6].is_nan());
        // Numa janela de 2 amostras (apenas os 2 últimos ticks ociosos), já não há histórico visível.
        assert!(!st.game_fps.has_finite_in(2));
        assert!(st.game_fps.has_finite_in(4));
    }
}
