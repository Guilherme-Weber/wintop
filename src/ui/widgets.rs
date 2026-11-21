//! Widgets no estilo do btop: gráfico em braille preenchido (2 amostras por célula, 4 níveis
//! por linha) e medidor de blocos com gradiente verde → amarelo → vermelho.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::Widget;

/// Gradiente do btop (padrão): verde em 0, amarelo no meio, vermelho em 1.
pub fn gradient(t: f64) -> Color {
    const LOW: (f64, f64, f64) = (80.0, 200.0, 120.0);
    const MID: (f64, f64, f64) = (230.0, 200.0, 60.0);
    const HIGH: (f64, f64, f64) = (230.0, 70.0, 70.0);
    let t = if t.is_finite() {
        t.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let (a, b, u) = if t < 0.5 {
        (LOW, MID, t * 2.0)
    } else {
        (MID, HIGH, (t - 0.5) * 2.0)
    };
    let mix = |x: f64, y: f64| (x + (y - x) * u).round() as u8;
    Color::Rgb(mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
}

/// Cor apagada das partes vazias.
pub const DIM: Color = Color::Rgb(60, 60, 70);

/// Bits do braille por linha de pontos (de baixo para cima), coluna esquerda e direita.
const LEFT: [u8; 4] = [0x40, 0x04, 0x02, 0x01];
const RIGHT: [u8; 4] = [0x80, 0x20, 0x10, 0x08];

/// Gráfico de área em braille. `samples` em 0..=1, o mais novo por último (fica à direita).
pub struct Graph<'a> {
    pub samples: &'a [f64],
    /// Cor fixa (ex.: rede); `None` = gradiente pela altura.
    pub color: Option<Color>,
}

/// Quantos pontos (de `0..=max`) acender para um valor em 0..=1: qualquer valor acima de zero
/// acende ao menos um ponto (atividade pequena não some).
fn dots(v: f64, max: usize) -> usize {
    if !v.is_finite() || v <= 0.0 {
        return 0;
    }
    ((v.min(1.0) * max as f64).round() as usize).clamp(1, max)
}

impl Widget for Graph<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let w = usize::from(area.width);
        let h = usize::from(area.height);
        let max = h * 4;
        // As últimas 2·w amostras, alinhadas à direita.
        let take = (w * 2).min(self.samples.len());
        let tail = &self.samples[self.samples.len() - take..];
        let offset = w * 2 - take;
        let level = |i: usize| -> usize {
            i.checked_sub(offset)
                .and_then(|k| tail.get(k))
                .map_or(0, |&v| dots(v, max))
        };
        for col in 0..w {
            let (l, r) = (level(col * 2), level(col * 2 + 1));
            for row in 0..h {
                // Linha 0 = a de baixo.
                let base = row * 4;
                let mut bits = 0u8;
                for d in 0..4 {
                    if l > base + d {
                        bits |= LEFT[d];
                    }
                    if r > base + d {
                        bits |= RIGHT[d];
                    }
                }
                if bits == 0 {
                    continue;
                }
                let ch = char::from_u32(0x2800 + u32::from(bits)).unwrap_or(' ');
                let color = self.color.unwrap_or_else(|| {
                    gradient(if h > 1 {
                        row as f64 / (h - 1) as f64
                    } else {
                        0.0
                    })
                });
                let pos = (area.x + col as u16, area.y + (h - 1 - row) as u16);
                if let Some(cell) = buf.cell_mut(pos) {
                    cell.set_char(ch).set_fg(color);
                }
            }
        }
    }
}

/// Medidor de blocos (`■`): parte cheia com o gradiente pela posição, resto apagado.
pub struct Meter {
    /// 0..=1.
    pub value: f64,
}

impl Widget for Meter {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let w = usize::from(area.width);
        if w == 0 || area.height == 0 {
            return;
        }
        let v = if self.value.is_finite() {
            self.value.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let filled = (v * w as f64).round() as usize;
        for i in 0..w {
            let style = if i < filled {
                Style::new().fg(gradient(i as f64 / w.max(2).saturating_sub(1) as f64))
            } else {
                Style::new().fg(DIM)
            };
            // Só dentro do buffer (área parcialmente fora da tela num terminal pequeno).
            if let Some(cell) = buf.cell_mut((area.x + i as u16, area.y)) {
                cell.set_symbol("■").set_style(style);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gradiente_nas_pontas() {
        assert_eq!(gradient(0.0), Color::Rgb(80, 200, 120));
        assert_eq!(gradient(0.5), Color::Rgb(230, 200, 60));
        assert_eq!(gradient(1.0), Color::Rgb(230, 70, 70));
        assert_eq!(gradient(f64::NAN), gradient(0.0));
    }

    #[test]
    fn pontos_minimos_e_maximos() {
        assert_eq!(dots(0.0, 8), 0);
        assert_eq!(dots(0.001, 8), 1);
        assert_eq!(dots(1.0, 8), 8);
        assert_eq!(dots(7.0, 8), 8);
    }

    #[test]
    fn grafico_alinhado_a_direita() {
        let area = Rect::new(0, 0, 3, 1);
        let mut buf = Buffer::empty(area);
        Graph {
            samples: &[1.0, 0.5],
            color: Some(Color::White),
        }
        .render(area, &mut buf);
        // 2 amostras ocupam só a última célula: 4 pontos à esquerda e 2 à direita.
        assert_eq!(buf[(0, 0)].symbol(), " ");
        assert_eq!(buf[(2, 0)].symbol(), "⣧");
    }

    #[test]
    fn medidor_pela_metade() {
        let area = Rect::new(0, 0, 4, 1);
        let mut buf = Buffer::empty(area);
        Meter { value: 0.5 }.render(area, &mut buf);
        assert_eq!(buf[(1, 0)].fg, gradient(1.0 / 3.0));
        assert_eq!(buf[(2, 0)].fg, DIM);
    }
}
