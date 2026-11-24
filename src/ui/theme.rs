//! Temas de cor (Etapa 7): paletas prontas, troca com a tecla `t` e cores avulsas do
//! `config.toml` (`[colors]`).
//!
//! O tema atual fica num `thread_local` da thread da interface (o desenho é single-thread; os
//! testes de snapshot, em threads próprias, usam o padrão). A falha continua vermelha em todos
//! os temas (regra do projeto: métrica sem dado = vermelho com a causa).

use std::cell::Cell;

use ratatui::style::Color;

/// Todas as cores da interface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    pub name: &'static str,
    pub border: Color,
    pub title: Color,
    pub text: Color,
    pub label: Color,
    /// Destaque semântico para valores principais (clocks, potência, link PCIe, flags).
    pub accent: Color,
    /// Estado positivo/saudável (all OK, virt enabled, throttle none, SMART OK).
    pub ok: Color,
    /// Atenção sem ser falha (throttle ativo, temperatura elevada, limitado pela GPU).
    pub warn: Color,
    pub neutral: Color,
    pub fail: Color,
    pub selected: Color,
    pub dim: Color,
    pub net_send: Color,
    pub net_recv: Color,
    pub fps: Color,
    pub frametime: Color,
    /// Picos do frametime (acima de `frametime_alert_ms`).
    pub frametime_spike: Color,
    /// FPS acima da taxa de atualização do monitor e abaixo de 30.
    pub fps_over: Color,
    pub fps_low: Color,
    pub encode: Color,
    pub decode: Color,
    pub ram_in_use: Color,
    pub ram_modified: Color,
    pub ram_standby: Color,
    pub game: Color,
    pub vram_app: Color,
    pub vram_compositor: Color,
    pub vram_browser: Color,
    pub vram_system: Color,
    pub vram_kernel: Color,
    /// Gradiente dos medidores e gráficos (baixo, meio, alto).
    pub low: Color,
    pub mid: Color,
    pub high: Color,
}

const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

pub const DEFAULT: Theme = Theme {
    name: "default",
    border: Color::Rgb(75, 82, 99),
    title: Color::Rgb(220, 224, 232),
    text: Color::Rgb(200, 204, 212),
    label: Color::Rgb(127, 133, 148),
    accent: rgb(0x93C5FD),
    ok: rgb(0x34D399),
    warn: rgb(0xFBBF24),
    neutral: Color::Rgb(108, 112, 128),
    fail: Color::Rgb(235, 80, 80),
    selected: Color::Rgb(48, 54, 70),
    // Trilha vazia dos medidores mais apagada que o topo do gradiente (cheio nunca parece vazio).
    dim: Color::Rgb(42, 44, 54),
    net_send: Color::Rgb(200, 120, 200),
    net_recv: Color::Rgb(92, 184, 228),
    // Cores do jogo escolhidas pelo usuário (2026-11-22).
    fps: rgb(0x108981),
    frametime: rgb(0x8386F1),
    frametime_spike: rgb(0xFF8A65),
    fps_over: rgb(0x1E1B4B),
    fps_low: rgb(0xFF8A65),
    encode: Color::Rgb(86, 194, 214),
    decode: Color::Rgb(176, 132, 224),
    ram_in_use: Color::Rgb(78, 184, 176),
    ram_modified: rgb(0xD4B85A),
    ram_standby: Color::Rgb(92, 120, 170),
    game: Color::Rgb(224, 138, 60),
    vram_app: Color::Rgb(78, 184, 176),
    vram_compositor: Color::Rgb(157, 124, 216),
    vram_browser: Color::Rgb(92, 156, 230),
    vram_system: Color::Rgb(154, 160, 176),
    vram_kernel: Color::Rgb(138, 132, 88),
    // Gradiente escolhido pelo usuário (2026-11-22): índigo escuro embaixo → violeta → coral
    // no alto.
    low: rgb(0x1E1B4B),
    mid: rgb(0x7E57C2),
    high: rgb(0xFF8A65),
};

const NORD: Theme = Theme {
    name: "nord",
    border: rgb(0x4c566a),
    title: rgb(0xeceff4),
    text: rgb(0xd8dee9),
    label: rgb(0x81a1c1),
    accent: rgb(0x88c0d0),
    ok: rgb(0xa3be8c),
    warn: rgb(0xebcb8b),
    neutral: rgb(0x616e88),
    fail: rgb(0xbf616a),
    selected: rgb(0x3b4252),
    dim: rgb(0x3b4252),
    net_send: rgb(0xb48ead),
    net_recv: rgb(0x88c0d0),
    fps: rgb(0xa3be8c),
    frametime: rgb(0xebcb8b),
    frametime_spike: rgb(0xbf616a),
    fps_over: rgb(0x616e88),
    fps_low: rgb(0xbf616a),
    encode: rgb(0x8fbcbb),
    decode: rgb(0xb48ead),
    ram_in_use: rgb(0x88c0d0),
    ram_modified: rgb(0xebcb8b),
    ram_standby: rgb(0x5e81ac),
    game: rgb(0xd08770),
    vram_app: rgb(0x8fbcbb),
    vram_compositor: rgb(0xb48ead),
    vram_browser: rgb(0x81a1c1),
    vram_system: rgb(0x7b88a1),
    vram_kernel: rgb(0xa89a6a),
    low: rgb(0xa3be8c),
    mid: rgb(0xebcb8b),
    high: rgb(0xbf616a),
};

const GRUVBOX: Theme = Theme {
    name: "gruvbox",
    border: rgb(0x504945),
    title: rgb(0xfbf1c7),
    text: rgb(0xebdbb2),
    label: rgb(0xa89984),
    accent: rgb(0x83a598),
    ok: rgb(0xb8bb26),
    warn: rgb(0xfabd2f),
    neutral: rgb(0x7c6f64),
    fail: rgb(0xfb4934),
    selected: rgb(0x3c3836),
    dim: rgb(0x3c3836),
    net_send: rgb(0xd3869b),
    net_recv: rgb(0x83a598),
    fps: rgb(0xb8bb26),
    frametime: rgb(0xfabd2f),
    frametime_spike: rgb(0xfb4934),
    fps_over: rgb(0x7c6f64),
    fps_low: rgb(0xfb4934),
    encode: rgb(0x8ec07c),
    decode: rgb(0xd3869b),
    ram_in_use: rgb(0x8ec07c),
    ram_modified: rgb(0xd79921),
    ram_standby: rgb(0x458588),
    game: rgb(0xfe8019),
    vram_app: rgb(0x8ec07c),
    vram_compositor: rgb(0xd3869b),
    vram_browser: rgb(0x83a598),
    vram_system: rgb(0xa89984),
    vram_kernel: rgb(0xd79921),
    low: rgb(0xb8bb26),
    mid: rgb(0xfabd2f),
    high: rgb(0xfb4934),
};

const DRACULA: Theme = Theme {
    name: "dracula",
    border: rgb(0x44475a),
    title: rgb(0xf8f8f2),
    text: rgb(0xe2e2dc),
    label: rgb(0x8b95c9),
    accent: rgb(0x8be9fd),
    ok: rgb(0x50fa7b),
    warn: rgb(0xf1fa8c),
    neutral: rgb(0x6272a4),
    fail: rgb(0xff5555),
    selected: rgb(0x44475a),
    dim: rgb(0x343746),
    net_send: rgb(0xff79c6),
    net_recv: rgb(0x8be9fd),
    fps: rgb(0x50fa7b),
    frametime: rgb(0xffb86c),
    frametime_spike: rgb(0xff5555),
    fps_over: rgb(0x6272a4),
    fps_low: rgb(0xff5555),
    encode: rgb(0x8be9fd),
    decode: rgb(0xbd93f9),
    ram_in_use: rgb(0x8be9fd),
    ram_modified: rgb(0xf1fa8c),
    ram_standby: rgb(0x6272a4),
    game: rgb(0xffb86c),
    vram_app: rgb(0x50fa7b),
    vram_compositor: rgb(0xbd93f9),
    vram_browser: rgb(0x8be9fd),
    vram_system: rgb(0x6272a4),
    vram_kernel: rgb(0xf1fa8c),
    low: rgb(0x50fa7b),
    mid: rgb(0xf1fa8c),
    high: rgb(0xff5555),
};

/// Tons de cinza; só a falha mantém a cor (vermelho).
const MONO: Theme = Theme {
    name: "mono",
    border: rgb(0x555555),
    title: rgb(0xf0f0f0),
    text: rgb(0xd0d0d0),
    label: rgb(0x8a8a8a),
    accent: rgb(0xe8e8e8),
    ok: rgb(0xd0d0d0),
    warn: rgb(0xf0f0f0),
    neutral: rgb(0x707070),
    fail: rgb(0xeb5050),
    selected: rgb(0x333333),
    dim: rgb(0x3a3a3a),
    net_send: rgb(0xc8c8c8),
    net_recv: rgb(0x9a9a9a),
    fps: rgb(0xd8d8d8),
    frametime: rgb(0xa8a8a8),
    frametime_spike: rgb(0xeb5050),
    fps_over: rgb(0x707070),
    fps_low: rgb(0xeb5050),
    encode: rgb(0xc0c0c0),
    decode: rgb(0x909090),
    ram_in_use: rgb(0xc8c8c8),
    ram_modified: rgb(0xa0a0a0),
    ram_standby: rgb(0x787878),
    game: rgb(0xffffff),
    vram_app: rgb(0xc8c8c8),
    vram_compositor: rgb(0xa8a8a8),
    vram_browser: rgb(0x909090),
    vram_system: rgb(0x787878),
    vram_kernel: rgb(0x606060),
    low: rgb(0x8a8a8a),
    mid: rgb(0xb8b8b8),
    high: rgb(0xffffff),
};

/// Temas prontos, na ordem da tecla `t`.
pub const ALL: [Theme; 5] = [DEFAULT, NORD, GRUVBOX, DRACULA, MONO];

thread_local! {
    static CURRENT: Cell<Theme> = const { Cell::new(DEFAULT) };
}

/// Tema atual (da thread da interface).
pub fn th() -> Theme {
    CURRENT.with(Cell::get)
}

pub fn set(t: Theme) {
    CURRENT.with(|c| c.set(t));
}

pub fn exists(name: &str) -> bool {
    ALL.iter().any(|t| t.name == name)
}

/// Tema pelo nome (padrão se não existir), com as cores avulsas do config aplicadas.
pub fn build(name: &str, overrides: &[(String, String)]) -> Theme {
    let mut t = ALL
        .iter()
        .copied()
        .find(|t| t.name == name)
        .unwrap_or(DEFAULT);
    for (k, hex) in overrides {
        if let (Some(c), Some(slot)) = (parse_hex(hex), slot(&mut t, k)) {
            *slot = c;
        }
    }
    t
}

/// Próximo tema da lista (tecla `t`).
pub fn next(name: &str) -> &'static str {
    let i = ALL.iter().position(|t| t.name == name).unwrap_or(0);
    ALL[(i + 1) % ALL.len()].name
}

/// `#rrggbb` → cor.
pub fn parse_hex(s: &str) -> Option<Color> {
    let h = s.strip_prefix('#')?;
    if h.len() != 6 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let v = u32::from_str_radix(h, 16).ok()?;
    Some(rgb(v))
}

pub fn is_color_name(name: &str) -> bool {
    slot(&mut DEFAULT.clone(), name).is_some()
}

fn slot<'a>(t: &'a mut Theme, name: &str) -> Option<&'a mut Color> {
    Some(match name {
        "border" => &mut t.border,
        "title" => &mut t.title,
        "text" => &mut t.text,
        "label" => &mut t.label,
        "accent" => &mut t.accent,
        "ok" => &mut t.ok,
        "warn" => &mut t.warn,
        "neutral" => &mut t.neutral,
        "fail" => &mut t.fail,
        "selected" => &mut t.selected,
        "dim" => &mut t.dim,
        "net_send" => &mut t.net_send,
        "net_recv" => &mut t.net_recv,
        "fps" => &mut t.fps,
        "frametime" => &mut t.frametime,
        "frametime_spike" => &mut t.frametime_spike,
        "fps_over" => &mut t.fps_over,
        "fps_low" => &mut t.fps_low,
        "encode" => &mut t.encode,
        "decode" => &mut t.decode,
        "ram_in_use" => &mut t.ram_in_use,
        "ram_modified" => &mut t.ram_modified,
        "ram_standby" => &mut t.ram_standby,
        "game" => &mut t.game,
        "vram_app" => &mut t.vram_app,
        "vram_compositor" => &mut t.vram_compositor,
        "vram_browser" => &mut t.vram_browser,
        "vram_system" => &mut t.vram_system,
        "vram_kernel" => &mut t.vram_kernel,
        "low" => &mut t.low,
        "mid" => &mut t.mid,
        "high" => &mut t.high,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzz_cor_hex() {
        crate::fuzz::run_text("cor_hex", &["#1a2b3c", "#FFFFFF"], 16, |s| {
            if parse_hex(s).is_some() {
                assert!(s.len() == 7 && s.starts_with('#'));
                assert!(s[1..].bytes().all(|b| b.is_ascii_hexdigit()));
            }
        });
    }

    #[test]
    fn cores_avulsas_e_ciclo() {
        assert_eq!(parse_hex("#4b5263"), Some(Color::Rgb(0x4b, 0x52, 0x63)));
        assert_eq!(parse_hex("4b5263"), None);
        assert_eq!(parse_hex("#4b526"), None);
        assert_eq!(parse_hex("#gg0000"), None);
        let t = build(
            "nord",
            &[
                ("border".into(), "#010203".into()),
                ("x".into(), "#000000".into()),
            ],
        );
        assert_eq!(t.border, Color::Rgb(1, 2, 3));
        assert_eq!(t.title, NORD.title);
        assert_eq!(build("nenhum", &[]), DEFAULT);
        assert_eq!(next("default"), "nord");
        assert_eq!(next("mono"), "default");
        // A falha é avermelhada em todos os temas.
        for t in ALL {
            let Color::Rgb(r, g, b) = t.fail else {
                panic!("{t:?}")
            };
            assert!(r > 180 && g < 120 && b < 120, "{}", t.name);
        }
    }
}
