//! Widgets no estilo do btop: gráfico e medidores em braille (2 amostras por célula, 4 níveis
//! por linha) com o gradiente do tema (baixo → meio → alto).

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::Widget;

fn rgb_f64(c: Color) -> (f64, f64, f64) {
    match c {
        Color::Rgb(r, g, b) => (f64::from(r), f64::from(g), f64::from(b)),
        _ => (128.0, 128.0, 128.0),
    }
}

fn gradient_rgb(
    t: f64,
    low: (f64, f64, f64),
    mid: (f64, f64, f64),
    high: (f64, f64, f64),
) -> Color {
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

/// Gradiente do tema entre baixo (0), meio (0,5) e alto (1).
pub fn gradient(t: f64) -> Color {
    let t3 = super::theme::th();
    gradient_rgb(t, rgb_f64(t3.low), rgb_f64(t3.mid), rgb_f64(t3.high))
}

/// O mesmo tom com metade do brilho (parte de kernel dos gráficos de CPU).
pub fn darker(c: Color) -> Color {
    match c {
        // Metade do brilho, mas o canal mais forte nunca abaixo de 64: a parte do kernel sobre
        // uma cor já escura (o topo do gradiente padrão) continua visível no fundo, e a
        // proporção entre os canais (a matiz, em qualquer tema) se mantém.
        Color::Rgb(r, g, b) => {
            let top = f64::from(r.max(g).max(b));
            let k = if top <= 0.0 {
                0.5
            } else {
                (64.0 / top).clamp(0.5, 1.0)
            };
            let f = |c: u8| (f64::from(c) * k).round() as u8;
            Color::Rgb(f(r), f(g), f(b))
        }
        other => other,
    }
}

/// Bits do braille por linha de pontos (de baixo para cima), coluna esquerda e direita.
const LEFT: [u8; 4] = [0x40, 0x04, 0x02, 0x01];
const RIGHT: [u8; 4] = [0x80, 0x20, 0x10, 0x08];

use super::glyphs::{self, Glyphs};

/// Níveis por linha de cada conjunto de símbolos.
fn levels(g: Glyphs) -> usize {
    match g {
        Glyphs::Braille => 4,
        Glyphs::Eighths => 8,
        Glyphs::Halves => 2,
    }
}

/// Oitavos de baixo para cima.
const LOWER8: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Bloco com `fill` níveis (1..=levels) acesos; `invert` cresce de cima para baixo (só há ▔, ▀ e
/// █ entre os blocos de cima).
fn block_char(g: Glyphs, fill: usize, invert: bool) -> char {
    match (g, invert) {
        (Glyphs::Halves | Glyphs::Braille, false) => {
            if fill >= 2 {
                '█'
            } else {
                '▄'
            }
        }
        (Glyphs::Halves | Glyphs::Braille, true) => {
            if fill >= 2 {
                '█'
            } else {
                '▀'
            }
        }
        (Glyphs::Eighths, false) => LOWER8[fill.min(8)],
        (Glyphs::Eighths, true) => match fill {
            0..=3 => '▔',
            4..=7 => '▀',
            _ => '█',
        },
    }
}

/// Linha de base (amostra zerada) em modo bloco: `_`/`¯` existem em toda fonte de console.
fn baseline_char(g: Glyphs, invert: bool) -> char {
    match (g, invert) {
        (Glyphs::Eighths, false) => '▁',
        (Glyphs::Eighths, true) => '▔',
        (_, false) => '_',
        (_, true) => '¯',
    }
}

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
    /// Cor fixa; `over` nas colunas acima do teto (amostra > 1) e `low` nas que ficam abaixo
    /// de `low_at` (FPS acima da taxa do monitor / abaixo de 30).
    Bands {
        base: Color,
        over: Color,
        low: Color,
        low_at: f64,
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
    /// Amostras zeradas acendem o ponto da base nesta cor (eixo da rede espelhada visível com
    /// a placa parada).
    pub baseline: Option<Color>,
    /// Escala das amostras (padrão 1.0): divide cada amostra sob demanda sem alocar `Vec`.
    pub scale: f64,
}

impl<'a> Graph<'a> {
    pub fn new(samples: &'a [f64], color: GraphColor) -> Self {
        Self {
            samples,
            color,
            invert: false,
            under: None,
            baseline: None,
            scale: 1.0,
        }
    }

    pub fn with_under(mut self, under: &'a [f64]) -> Self {
        self.under = Some(under);
        self
    }

    pub fn with_baseline(mut self, c: Color) -> Self {
        self.baseline = Some(c);
        self
    }

    pub fn with_scale(mut self, scale: f64) -> Self {
        self.scale = scale;
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
        let g = glyphs::get();
        let braille = g == Glyphs::Braille;
        let lv = levels(g);
        let max = h * lv;
        let norm = |v: f64| -> f64 {
            if self.scale == 1.0 {
                v
            } else if self.scale > 0.0 {
                v / self.scale
            } else {
                0.0
            }
        };
        // As últimas 2·w amostras, alinhadas à direita.
        let take = (w * 2).min(self.samples.len());
        let tail = &self.samples[self.samples.len() - take..];
        let offset = w * 2 - take;
        let sample = |i: usize| -> f64 {
            i.checked_sub(offset)
                .and_then(|k| tail.get(k))
                .map_or(0.0, |&v| norm(v))
        };
        // A série de baixo, alinhada à direita do mesmo jeito.
        let under = self.under.map(|u| &u[u.len() - (w * 2).min(u.len())..]);
        let under_dots = |i: usize| -> usize {
            under.map_or(0, |u| {
                let off = w * 2 - u.len();
                i.checked_sub(off)
                    .and_then(|k| u.get(k))
                    .map_or(0, |&v| dots(norm(v), max))
            })
        };
        // Pré-calcula o gradiente fora do laço por célula (evita w×h leituras do tema).
        let grad_stops = matches!(self.color, GraphColor::Height | GraphColor::Value).then(|| {
            let t3 = super::theme::th();
            (rgb_f64(t3.low), rgb_f64(t3.mid), rgb_f64(t3.high))
        });
        let mut row_colors = [Color::Reset; 64];
        if matches!(self.color, GraphColor::Height)
            && h > 1
            && let Some((low, mid, high)) = grad_stops
        {
            for (row, slot) in row_colors.iter_mut().enumerate().take(h) {
                *slot = gradient_rgb(row as f64 / (h - 1) as f64, low, mid, high);
            }
        }
        for col in 0..w {
            let (sl, sr) = (sample(col * 2), sample(col * 2 + 1));
            let peak = sl.max(sr);
            // Em blocos a célula tem uma coluna só: a maior das duas amostras (a mesma janela de
            // tempo do braille, sem perder picos).
            let (l, r) = if braille {
                (dots(sl, max), dots(sr, max))
            } else {
                let n = dots(peak, max);
                (n, n)
            };
            let col_color = match self.color {
                GraphColor::Height if h > 1 => Color::Reset,
                GraphColor::Height | GraphColor::Value => {
                    let (low, mid, high) =
                        grad_stops.unwrap_or(((0.0, 0.0, 0.0), (0.0, 0.0, 0.0), (0.0, 0.0, 0.0)));
                    gradient_rgb(0.25 + 0.75 * peak, low, mid, high)
                }
                GraphColor::Fixed(c) => c,
                GraphColor::Threshold { base, alert, at } => {
                    if peak >= at {
                        alert
                    } else {
                        base
                    }
                }
                GraphColor::Bands {
                    base,
                    over,
                    low,
                    low_at,
                } => {
                    if peak > 1.0 {
                        over
                    } else if peak < low_at {
                        low
                    } else {
                        base
                    }
                }
            };
            // Kernel da coluna: o menor dos dois (a célula só escurece se estiver toda por baixo).
            let k = under_dots(col * 2).min(under_dots(col * 2 + 1));
            for row in 0..h {
                // Linha 0 = a da base (a de baixo; a de cima se invertido).
                let base = row * lv;
                let block = (!braille).then(|| {
                    let fill = l.saturating_sub(base).min(lv);
                    if fill > 0 {
                        Some((block_char(g, fill, self.invert), false))
                    } else if row == 0 && self.baseline.is_some() {
                        Some((baseline_char(g, self.invert), true))
                    } else {
                        None
                    }
                });
                let (ch, base_only) = match block {
                    Some(Some(b)) => b,
                    Some(None) => continue,
                    None => match braille_cell(l, r, base, row, self.invert, self.baseline) {
                        Some(b) => b,
                        None => continue,
                    },
                };
                let color = if matches!(self.color, GraphColor::Height) && h > 1 {
                    if row < row_colors.len() {
                        row_colors[row]
                    } else if let Some((low, mid, high)) = grad_stops {
                        gradient_rgb(row as f64 / (h - 1) as f64, low, mid, high)
                    } else {
                        col_color
                    }
                } else {
                    col_color
                };
                // Pelo menos metade da célula na parte de kernel: o mesmo tom, mais escuro (com a
                // célula inteira, kernel baixo nunca apareceria na grade por núcleo).
                let color = if k >= base + lv / 2 {
                    darker(color)
                } else {
                    color
                };
                let color = match self.baseline {
                    Some(b) if base_only => b,
                    _ => color,
                };
                let y = if self.invert { row } else { h - 1 - row };
                if let Some(cell) = buf.cell_mut((area.x + col as u16, area.y + y as u16)) {
                    cell.set_char(ch).set_fg(color);
                }
            }
        }
    }
}

/// Célula de braille da linha `row` (pontos de `base` a `base + 3`): o caractere e se ele só
/// tem a linha de base. `None` = célula vazia.
fn braille_cell(
    l: usize,
    r: usize,
    base: usize,
    row: usize,
    invert: bool,
    baseline: Option<Color>,
) -> Option<(char, bool)> {
    let mut bits = 0u8;
    for d in 0..4 {
        // Invertido, os pontos de cada célula também enchem de cima para baixo.
        let (lb, rb) = if invert {
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
    // Base das amostras zeradas (só a 1ª linha de pontos, na cor da base).
    let mut base_only = false;
    if row == 0 && baseline.is_some() {
        let (lb, rb) = if invert {
            (LEFT[3], RIGHT[3])
        } else {
            (LEFT[0], RIGHT[0])
        };
        base_only = bits == 0;
        if l == 0 {
            bits |= lb;
        }
        if r == 0 {
            bits |= rb;
        }
    }
    (bits != 0).then(|| {
        (
            char::from_u32(0x2800 + u32::from(bits)).unwrap_or(' '),
            base_only,
        )
    })
}

/// Glifo da trilha apagada de medidores: em temas coloridos usa `⣿` em `dim` (estilo btop);
/// sob `NO_COLOR` ou tema `mono`, usa `⣀` para manter contraste visual sem depender de cor.
fn empty_track_glyph() -> &'static str {
    if glyphs::get() != Glyphs::Braille {
        // Em blocos: sombra leve (WGL4), que se distingue do cheio mesmo sem cor.
        "░"
    } else if crate::ui::theme::no_color() || crate::ui::theme::th().name == "mono" {
        "⣀"
    } else {
        "⣿"
    }
}

/// Medidor horizontal em braille (`⡇`, `⣿`): parte cheia com o gradiente pela posição (resolução
/// de 2 colunas de pontos por célula) e trilha apagada (`⣿` em `dim`, ou `⣀` em `mono`/`NO_COLOR`).
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
        let max_steps = w * 2;
        let steps = if v <= 0.0 {
            0
        } else {
            ((v * max_steps as f64).round() as usize).clamp(1, max_steps)
        };
        let t3 = crate::ui::theme::th();
        let dim = t3.dim;
        let empty = empty_track_glyph();
        let (low, mid, high) = (rgb_f64(t3.low), rgb_f64(t3.mid), rgb_f64(t3.high));
        let denom = w.max(2).saturating_sub(1) as f64;
        let (half, full) = if glyphs::get() == Glyphs::Braille {
            ("⡇", "⣿")
        } else {
            ("▌", "█")
        };
        for i in 0..w {
            let fill = steps.saturating_sub(i * 2).min(2);
            let (sym, style) = match fill {
                0 => (empty, Style::new().fg(dim)),
                1 => (
                    half,
                    Style::new().fg(gradient_rgb(i as f64 / denom, low, mid, high)),
                ),
                _ => (
                    full,
                    Style::new().fg(gradient_rgb(i as f64 / denom, low, mid, high)),
                ),
            };
            // Só dentro do buffer (área parcialmente fora da tela num terminal pequeno).
            if let Some(cell) = buf.cell_mut((area.x + i as u16, area.y)) {
                cell.set_symbol(sym).set_style(style);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gradiente_nas_pontas() {
        assert_eq!(gradient(0.0), Color::Rgb(0x1E, 0x1B, 0x4B));
        assert_eq!(gradient(0.5), Color::Rgb(0x7E, 0x57, 0xC2));
        assert_eq!(gradient(1.0), Color::Rgb(0xFF, 0x8A, 0x65));
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
    fn linha_de_base_nas_amostras_zeradas() {
        let area = Rect::new(0, 0, 2, 2);
        let mut buf = Buffer::empty(area);
        let (c, b) = (Color::Rgb(200, 0, 0), Color::Rgb(9, 9, 9));
        // Célula 0 zerada: só a base (⣀), na cor da base. Célula 1 com valor: cor normal.
        Graph::new(&[0.0, 0.0, 0.5, 0.0], GraphColor::Fixed(c))
            .with_baseline(b)
            .render(area, &mut buf);
        assert_eq!(buf[(0, 1)].symbol(), "⣀");
        assert_eq!(buf[(0, 1)].fg, b);
        assert_eq!(buf[(1, 1)].fg, c);
        assert_eq!(buf[(0, 0)].symbol(), " ");
        // Invertido: a base fica na linha de cima (⠉).
        let mut buf = Buffer::empty(area);
        Graph::new(&[0.0, 0.0], GraphColor::Fixed(c))
            .with_baseline(b)
            .inverted()
            .render(area, &mut buf);
        assert_eq!(buf[(1, 0)].symbol(), "⠉");
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
        // Valor pequeno parte de um quarto da escala (não some no fundo).
        let mut buf = Buffer::empty(area);
        Graph::new(&[0.01, 0.0], GraphColor::Value).render(area, &mut buf);
        assert_eq!(buf[(0, 0)].fg, gradient(0.25 + 0.75 * 0.01));
        let mut buf = Buffer::empty(area);
        let t = GraphColor::Threshold {
            base: Color::Green,
            alert: Color::Red,
            at: 0.66,
        };
        Graph::new(&[0.7, 0.1], t).render(area, &mut buf);
        assert_eq!(buf[(0, 0)].fg, Color::Red);
        // Faixas: acima do teto, normal e abaixo do limite.
        let b = GraphColor::Bands {
            base: Color::Green,
            over: Color::Blue,
            low: Color::Red,
            low_at: 0.125,
        };
        for (s, c) in [(1.2, Color::Blue), (0.5, Color::Green), (0.1, Color::Red)] {
            let mut buf = Buffer::empty(area);
            Graph::new(&[s, s], b).render(area, &mut buf);
            assert_eq!(buf[(0, 0)].fg, c, "{s}");
        }
    }

    #[test]
    fn medidor_pela_metade() {
        let area = Rect::new(0, 0, 4, 1);
        let mut buf = Buffer::empty(area);
        Meter { value: 0.5 }.render(area, &mut buf);
        assert_eq!(buf[(1, 0)].symbol(), "⣿");
        assert_eq!(buf[(1, 0)].fg, gradient(1.0 / 3.0));
        assert_eq!(buf[(2, 0)].symbol(), "⣿");
        assert_eq!(buf[(2, 0)].fg, crate::ui::theme::th().dim);
        // Valor pequeno positivo acende meia célula (`⡇`) na esquerda.
        let mut buf = Buffer::empty(area);
        Meter { value: 0.001 }.render(area, &mut buf);
        assert_eq!(buf[(0, 0)].symbol(), "⡇");
        assert_eq!(buf[(0, 0)].fg, gradient(0.0));
        assert_eq!(buf[(1, 0)].fg, crate::ui::theme::th().dim);
    }

    #[test]
    fn barra_vertical_preenche_de_baixo_para_cima() {
        let area = Rect::new(0, 0, 2, 4);
        let mut buf = Buffer::empty(area);
        VerticalBar { value: 0.5 }.render(area, &mut buf);
        // Metade de 4 linhas = 2 linhas cheias embaixo (`⣿`), 2 apagadas em cima (`⣿` em `dim`).
        assert_eq!(buf[(0, 3)].symbol(), "⣿");
        assert_eq!(buf[(1, 2)].symbol(), "⣿");
        assert_eq!(buf[(0, 1)].symbol(), "⣿");
        assert_eq!(buf[(1, 0)].symbol(), "⣿");
        assert_eq!(buf[(0, 0)].fg, crate::ui::theme::th().dim);
        assert_eq!(buf[(0, 3)].fg, gradient(0.0));
        // NaN e zero: tudo na trilha apagada.
        let mut buf = Buffer::empty(area);
        VerticalBar { value: f64::NAN }.render(area, &mut buf);
        assert_eq!(buf[(0, 3)].symbol(), "⣿");
        assert_eq!(buf[(0, 3)].fg, crate::ui::theme::th().dim);
        // Valor mínimo positivo acende o menor degrau braille na base.
        let mut buf = Buffer::empty(area);
        VerticalBar { value: 0.001 }.render(area, &mut buf);
        assert_eq!(buf[(0, 3)].symbol(), "⣀");
        // Cheio: todas as linhas preenchidas, topo na cor alta do gradiente.
        let mut buf = Buffer::empty(area);
        VerticalBar { value: 1.0 }.render(area, &mut buf);
        assert_eq!(buf[(0, 0)].symbol(), "⣿");
        assert_eq!(buf[(0, 0)].fg, gradient(1.0));
    }

    #[test]
    fn escala_do_grafico_sem_alocar() {
        let area = Rect::new(0, 0, 2, 2);
        let mut buf = Buffer::empty(area);
        Graph::new(&[200.0, 200.0], GraphColor::Fixed(Color::Rgb(200, 100, 50)))
            .with_under(&[100.0, 100.0])
            .with_scale(200.0)
            .render(area, &mut buf);
        // Igual a [1.0, 1.0] com under [0.5, 0.5]: linha de baixo escurece, de cima normal.
        assert_eq!(buf[(1, 1)].fg, Color::Rgb(100, 50, 25));
        assert_eq!(buf[(1, 0)].fg, Color::Rgb(200, 100, 50));
    }
}

/// Barra cortada em partes coloridas (VRAM por categoria, como o gputaskman). Cada célula pega a
/// cor da parte que cobre o seu meio; o que sobra do total fica apagado.
pub struct Segmented<'a> {
    pub parts: &'a [(u64, Color)],
    pub total: u64,
}

/// Barra vertical preenchida de baixo para cima em braille (`⣀`, `⣤`, `⣶`, `⣿`) com o gradiente
/// do tema pela altura (termômetro de CPU/GPU, mesmo padrão visual dos gráficos).
pub struct VerticalBar {
    /// 0..=1.
    pub value: f64,
}

const VBRAILLE: [&str; 5] = ["⣿", "⣀", "⣤", "⣶", "⣿"];

impl Widget for VerticalBar {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let w = usize::from(area.width);
        let h = usize::from(area.height);
        if w == 0 || h == 0 {
            return;
        }
        let v = if self.value.is_finite() {
            self.value.clamp(0.0, 1.0)
        } else {
            0.0
        };
        // Resolução de 4 fileiras de pontos braille por linha (8 ou 2 em blocos); qualquer valor
        // > 0 acende a base.
        let g = glyphs::get();
        let lv = levels(g);
        let max_steps = h * lv;
        let steps = if v <= 0.0 {
            0
        } else {
            ((v * max_steps as f64).round() as usize).clamp(1, max_steps)
        };
        let t3 = crate::ui::theme::th();
        let dim = t3.dim;
        let empty = empty_track_glyph();
        let (low, mid, high) = (rgb_f64(t3.low), rgb_f64(t3.mid), rgb_f64(t3.high));
        for row in 0..h {
            // `row` = 0 na base, `h - 1` no topo.
            let base = row * lv;
            let fill = steps.saturating_sub(base).min(lv);
            let t = if h > 1 {
                row as f64 / (h - 1) as f64
            } else {
                v
            };
            let mut tmp = [0u8; 4];
            let (sym, style) = if fill == 0 {
                (empty, Style::new().fg(dim))
            } else if g == Glyphs::Braille {
                (
                    VBRAILLE[fill],
                    Style::new().fg(gradient_rgb(t, low, mid, high)),
                )
            } else {
                (
                    &*block_char(g, fill, false).encode_utf8(&mut tmp),
                    Style::new().fg(gradient_rgb(t, low, mid, high)),
                )
            };
            let y = area.y + (h - 1 - row) as u16;
            for col in 0..w {
                if let Some(cell) = buf.cell_mut((area.x + col as u16, y)) {
                    cell.set_symbol(sym).set_style(style);
                }
            }
        }
    }
}

impl Widget for Segmented<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let w = usize::from(area.width);
        if w == 0 || area.height == 0 || self.total == 0 {
            return;
        }
        let dim = crate::ui::theme::th().dim;
        let empty = empty_track_glyph();
        let full = if glyphs::get() == Glyphs::Braille {
            "⣿"
        } else {
            "█"
        };
        for i in 0..w {
            // Meio da célula, em bytes.
            let mid = ((i as f64 + 0.5) / w as f64) * self.total as f64;
            let mut acc = 0.0;
            let mut color = dim;
            let mut filled = false;
            for &(bytes, c) in self.parts {
                acc += bytes as f64;
                if mid < acc {
                    color = c;
                    filled = true;
                    break;
                }
            }
            if let Some(cell) = buf.cell_mut((area.x + i as u16, area.y)) {
                let sym = if filled { full } else { empty };
                cell.set_symbol(sym).set_style(Style::new().fg(color));
            }
        }
    }
}

#[cfg(test)]
mod block_tests {
    use super::*;

    /// Fonte sem braille (Consolas): gráfico, medidor, barra vertical e segmentada só com os
    /// blocos do WGL4; o gráfico junta as duas amostras da célula pelo máximo.
    #[test]
    fn modo_meio_bloco() {
        glyphs::set(Glyphs::Halves);
        let area = Rect::new(0, 0, 2, 2);
        let mut buf = Buffer::empty(area);
        Graph::new(&[0.0, 1.0, 0.3, 0.0], GraphColor::Fixed(Color::Red)).render(area, &mut buf);
        assert_eq!(buf[(0, 1)].symbol(), "█");
        assert_eq!(buf[(0, 0)].symbol(), "█");
        assert_eq!(buf[(1, 1)].symbol(), "▄");
        assert_eq!(buf[(1, 0)].symbol(), " ");
        // Linha de base nas amostras zeradas; invertido, cresce de cima.
        let mut buf = Buffer::empty(area);
        Graph::new(&[0.0, 0.0, 0.2, 0.2], GraphColor::Fixed(Color::Red))
            .with_baseline(Color::Blue)
            .inverted()
            .render(area, &mut buf);
        assert_eq!(buf[(0, 0)].symbol(), "¯");
        assert_eq!(buf[(0, 0)].fg, Color::Blue);
        assert_eq!(buf[(1, 0)].symbol(), "▀");
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 1));
        Meter { value: 0.375 }.render(Rect::new(0, 0, 4, 1), &mut buf);
        let row: String = (0..4).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        assert_eq!(row, "█▌░░");
        let mut buf = Buffer::empty(Rect::new(0, 0, 1, 2));
        VerticalBar { value: 0.25 }.render(Rect::new(0, 0, 1, 2), &mut buf);
        assert_eq!(buf[(0, 1)].symbol(), "▄");
        assert_eq!(buf[(0, 0)].symbol(), "░");
        let mut buf = Buffer::empty(Rect::new(0, 0, 2, 1));
        Segmented {
            parts: &[(1, Color::Red)],
            total: 2,
        }
        .render(Rect::new(0, 0, 2, 1), &mut buf);
        assert_eq!(buf[(0, 0)].symbol(), "█");
        assert_eq!(buf[(1, 0)].symbol(), "░");
        glyphs::set(Glyphs::Braille);
    }

    #[test]
    fn modo_oitavos() {
        glyphs::set(Glyphs::Eighths);
        let area = Rect::new(0, 0, 1, 1);
        let mut buf = Buffer::empty(area);
        Graph::new(&[0.5, 0.25], GraphColor::Fixed(Color::Red)).render(area, &mut buf);
        assert_eq!(buf[(0, 0)].symbol(), "▄");
        glyphs::set(Glyphs::Braille);
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
        assert_eq!(buf[(0, 0)].symbol(), "⣿");
        assert_eq!(buf[(0, 0)].fg, a);
        assert_eq!(buf[(2, 0)].fg, a);
        assert_eq!(buf[(3, 0)].fg, b);
        assert_eq!(buf[(4, 0)].fg, b);
        assert_eq!(buf[(5, 0)].symbol(), "⣿");
        assert_eq!(buf[(5, 0)].fg, crate::ui::theme::th().dim);
    }
}
