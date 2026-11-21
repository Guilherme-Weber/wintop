//! Widgets no estilo do btop: gráfico em braille preenchido (2 amostras por célula, 4 níveis
//! por linha) e medidor de blocos com gradiente verde → amarelo → vermelho.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::Widget;

/// Gradiente do btop (padrão): verde em 0, amarelo no meio, vermelho em 1.
pub fn gradient(t: f64) -> Color {
    let rgb = |c: Color| match c {
        Color::Rgb(r, g, b) => (f64::from(r), f64::from(g), f64::from(b)),
        _ => (128.0, 128.0, 128.0),
    };
    let t3 = super::theme::th();
    let (low, mid, high) = (rgb(t3.low), rgb(t3.mid), rgb(t3.high));
    let t = if t.is_finite() {
        t.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let (a, b, u) = if t < 0.5 {
        (low, mid, t * 2.0)
    } else {
        (mid, high, (t - 0.5) * 2.0)
    };
    let mix = |x: f64, y: f64| (x + (y - x) * u).round() as u8;
    Color::Rgb(mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
}

/// O mesmo tom com metade do brilho (parte de kernel dos gráficos de CPU).
pub fn darker(c: Color) -> Color {
    match c {
        Color::Rgb(r, g, b) => Color::Rgb(r / 2, g / 2, b / 2),
        other => other,
    }
}

/// Bits do braille por linha de pontos (de baixo para cima), coluna esquerda e direita.
const LEFT: [u8; 4] = [0x40, 0x04, 0x02, 0x01];
const RIGHT: [u8; 4] = [0x80, 0x20, 0x10, 0x08];

/// Como colorir o gráfico.
#[derive(Clone, Copy)]
pub enum GraphColor {
    /// Gradiente pela altura da linha (gráficos altos: CPU, GPU).
    Height,
    /// Gradiente pelo valor da amostra (gráficos de 1 linha: discos).
    Value,
    Fixed(Color),
    /// Cor fixa, e `alert` nas colunas com valor ≥ `at` (frametime acima de 33 ms).
    Threshold {
        base: Color,
        alert: Color,
        at: f64,
    },
}

/// Gráfico de área em braille. `samples` em 0..=1, o mais novo por último (fica à direita).
/// `invert`: cresce de cima para baixo (metade de baixo da rede espelhada).
pub struct Graph<'a> {
    pub samples: &'a [f64],
    pub color: GraphColor,
    pub invert: bool,
    /// Parte de cada amostra pintada num tom mais escuro (tempo de kernel, como o Task Manager
    /// com "Show kernel times"), na mesma escala e alinhamento de `samples`.
    pub under: Option<&'a [f64]>,
}

impl<'a> Graph<'a> {
    pub fn new(samples: &'a [f64], color: GraphColor) -> Self {
        Self {
            samples,
            color,
            invert: false,
            under: None,
        }
    }

    pub fn with_under(mut self, under: &'a [f64]) -> Self {
        self.under = Some(under);
        self
    }

    pub fn inverted(mut self) -> Self {
        self.invert = true;
        self
    }
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
        let sample = |i: usize| -> f64 {
            i.checked_sub(offset)
                .and_then(|k| tail.get(k))
                .copied()
                .unwrap_or(0.0)
        };
        // A série de baixo, alinhada à direita do mesmo jeito.
        let under = self.under.map(|u| &u[u.len() - (w * 2).min(u.len())..]);
        let under_dots = |i: usize| -> usize {
            under.map_or(0, |u| {
                let off = w * 2 - u.len();
                i.checked_sub(off)
                    .and_then(|k| u.get(k))
                    .map_or(0, |&v| dots(v, max))
            })
        };
        for col in 0..w {
            let (sl, sr) = (sample(col * 2), sample(col * 2 + 1));
            let (l, r) = (dots(sl, max), dots(sr, max));
            let peak = sl.max(sr);
            // Kernel da coluna: o menor dos dois (a célula só escurece se estiver toda por baixo).
            let k = under_dots(col * 2).min(under_dots(col * 2 + 1));
            for row in 0..h {
                // Linha 0 = a da base (a de baixo; a de cima se invertido).
                let base = row * 4;
                let mut bits = 0u8;
                for d in 0..4 {
                    // Invertido, os pontos de cada célula também enchem de cima para baixo.
                    let (lb, rb) = if self.invert {
                        (LEFT[3 - d], RIGHT[3 - d])
                    } else {
                        (LEFT[d], RIGHT[d])
                    };
                    if l > base + d {
                        bits |= lb;
                    }
                    if r > base + d {
                        bits |= rb;
                    }
                }
                if bits == 0 {
                    continue;
                }
                let ch = char::from_u32(0x2800 + u32::from(bits)).unwrap_or(' ');
                let color = match self.color {
                    GraphColor::Height => gradient(if h > 1 {
                        row as f64 / (h - 1) as f64
                    } else {
                        0.0
                    }),
                    GraphColor::Value => gradient(peak),
                    GraphColor::Fixed(c) => c,
                    GraphColor::Threshold { base, alert, at } => {
                        if peak >= at {
                            alert
                        } else {
                            base
                        }
                    }
                };
                // Pelo menos metade da célula na parte de kernel: o mesmo tom, mais escuro (com a
                // célula inteira, kernel baixo nunca apareceria na grade por núcleo).
                let color = if k >= base + 2 { darker(color) } else { color };
                let y = if self.invert { row } else { h - 1 - row };
                if let Some(cell) = buf.cell_mut((area.x + col as u16, area.y + y as u16)) {
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
                Style::new().fg(crate::ui::theme::th().dim)
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
        Graph::new(&[1.0, 0.5], GraphColor::Fixed(Color::White)).render(area, &mut buf);
        // 2 amostras ocupam só a última célula: 4 pontos à esquerda e 2 à direita.
        assert_eq!(buf[(0, 0)].symbol(), " ");
        assert_eq!(buf[(2, 0)].symbol(), "⣧");
        // Invertido: os 4 pontos à esquerda e os 2 de cima à direita (0x47 | 0x18 = ⡟).
        let mut buf = Buffer::empty(area);
        Graph::new(&[1.0, 0.5], GraphColor::Fixed(Color::White))
            .inverted()
            .render(area, &mut buf);
        assert_eq!(buf[(2, 0)].symbol(), "⡟");
    }

    #[test]
    fn parte_de_kernel_mais_escura() {
        // 2 linhas (8 pontos); uso 100%, kernel 50%: a linha de baixo escurece, a de cima não.
        let area = Rect::new(0, 0, 1, 2);
        let mut buf = Buffer::empty(area);
        Graph::new(&[1.0, 1.0], GraphColor::Fixed(Color::Rgb(200, 100, 50)))
            .with_under(&[0.5, 0.5])
            .render(area, &mut buf);
        assert_eq!(buf[(0, 1)].fg, Color::Rgb(100, 50, 25));
        assert_eq!(buf[(0, 0)].fg, Color::Rgb(200, 100, 50));
    }

    #[test]
    fn cor_pelo_valor_e_limite() {
        let area = Rect::new(0, 0, 1, 1);
        let mut buf = Buffer::empty(area);
        Graph::new(&[1.0, 1.0], GraphColor::Value).render(area, &mut buf);
        assert_eq!(buf[(0, 0)].fg, gradient(1.0));
        let mut buf = Buffer::empty(area);
        let t = GraphColor::Threshold {
            base: Color::Green,
            alert: Color::Red,
            at: 0.66,
        };
        Graph::new(&[0.7, 0.1], t).render(area, &mut buf);
        assert_eq!(buf[(0, 0)].fg, Color::Red);
    }

    #[test]
    fn medidor_pela_metade() {
        let area = Rect::new(0, 0, 4, 1);
        let mut buf = Buffer::empty(area);
        Meter { value: 0.5 }.render(area, &mut buf);
        assert_eq!(buf[(1, 0)].fg, gradient(1.0 / 3.0));
        assert_eq!(buf[(2, 0)].fg, crate::ui::theme::th().dim);
    }
}

/// Barra cortada em partes coloridas (VRAM por categoria, como o gputaskman). Cada célula pega a
/// cor da parte que cobre o seu meio; o que sobra do total fica apagado.
pub struct Segmented<'a> {
    pub parts: &'a [(u64, Color)],
    pub total: u64,
}

impl Widget for Segmented<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let w = usize::from(area.width);
        if w == 0 || area.height == 0 || self.total == 0 {
            return;
        }
        for i in 0..w {
            // Meio da célula, em bytes.
            let mid = ((i as f64 + 0.5) / w as f64) * self.total as f64;
            let mut acc = 0.0;
            let mut color = crate::ui::theme::th().dim;
            for &(bytes, c) in self.parts {
                acc += bytes as f64;
                if mid < acc {
                    color = c;
                    break;
                }
            }
            if let Some(cell) = buf.cell_mut((area.x + i as u16, area.y)) {
                cell.set_symbol("■").set_style(Style::new().fg(color));
            }
        }
    }
}

#[cfg(test)]
mod segmented_tests {
    use super::*;

    #[test]
    fn partes_proporcionais() {
        let area = Rect::new(0, 0, 10, 1);
        let mut buf = Buffer::empty(area);
        let (a, b) = (Color::Red, Color::Blue);
        Segmented {
            parts: &[(3, a), (2, b)],
            total: 10,
        }
        .render(area, &mut buf);
        assert_eq!(buf[(0, 0)].fg, a);
        assert_eq!(buf[(2, 0)].fg, a);
        assert_eq!(buf[(3, 0)].fg, b);
        assert_eq!(buf[(4, 0)].fg, b);
        assert_eq!(buf[(5, 0)].fg, crate::ui::theme::th().dim);
    }
}
