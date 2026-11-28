//! Símbolos dos gráficos e medidores conforme a fonte do console.
//!
//! Os widgets desenham em braille (U+2800) como o btop. O Windows Terminal faz fallback de
//! fonte, mas o console clássico (conhost) não: com uma fonte sem braille (a Consolas, padrão do
//! Windows 10) cada célula vira um quadrado vazio. Na partida, no conhost, a fonte é conferida
//! pela GDI (`GetGlyphIndicesW`); sem braille, o wintop troca a fonte do console por uma
//! monoespaçada instalada que tenha braille (a original volta na saída) e, sem nenhuma, desenha
//! com blocos (oitavos ▁..█ quando a fonte os tem; senão só ▄ ▀ █, presentes em toda fonte de
//! console por fazerem parte do WGL4).

use std::cell::Cell;

/// Conjunto de símbolos em uso pelos widgets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Glyphs {
    /// Braille: 2 amostras por célula e 4 níveis por linha.
    #[default]
    Braille,
    /// Blocos em oitavos (▁▂▃▄▅▆▇█): 1 célula por 2 amostras e 8 níveis por linha.
    Eighths,
    /// Só meio bloco e bloco cheio (▄ █): 2 níveis por linha.
    Halves,
}

/// Preferência do `config.toml` (`graph_symbols`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Symbols {
    /// Braille se a fonte tiver (ou trocar a fonte do conhost); senão blocos.
    #[default]
    Auto,
    Braille,
    Block,
}

impl Symbols {
    pub fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Braille => "braille",
            Self::Block => "block",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(Self::Auto),
            "braille" => Some(Self::Braille),
            "block" => Some(Self::Block),
            _ => None,
        }
    }
}

thread_local! {
    static CURRENT: Cell<Glyphs> = const { Cell::new(Glyphs::Braille) };
}

/// Símbolos da thread da interface (os testes, em threads próprias, usam braille).
pub fn get() -> Glyphs {
    CURRENT.with(Cell::get)
}

pub fn set(g: Glyphs) {
    CURRENT.with(|c| c.set(g));
}

/// Os símbolos de cada conjunto (o que a fonte precisa ter).
const BRAILLE: &str = "\u{28FF}\u{2847}\u{2840}\u{28C0}\u{28E4}\u{28F6}\u{2809}";
const EIGHTHS: &str = "▁▂▃▄▅▆▇█▔▀▌░";

/// Fontes monoespaçadas com braille que o conhost aceita, por preferência (a cobertura é
/// conferida pela GDI antes da troca).
const CANDIDATES: [&str; 9] = [
    "Cascadia Mono",
    "Cascadia Code",
    "DejaVu Sans Mono",
    "JetBrains Mono",
    "Fira Mono",
    "Fira Code",
    "Hack",
    "Iosevka",
    "Noto Sans Mono",
];

/// A fonte original do console, devolvida no Drop.
pub struct FontRestore {
    original: windows::Win32::System::Console::CONSOLE_FONT_INFOEX,
}

impl Drop for FontRestore {
    fn drop(&mut self) {
        if let Some(out) = console_out() {
            // SAFETY: handle do console deste processo; estrutura lida dele mesmo.
            unsafe {
                let _ = windows::Win32::System::Console::SetCurrentConsoleFontEx(
                    out,
                    false,
                    &raw const self.original,
                );
            }
        }
    }
}

/// Decide os símbolos e, se preciso, troca a fonte do conhost. Devolve os símbolos, a guarda
/// que restaura a fonte original e uma nota para o log de depuração.
pub fn prepare(pref: Symbols) -> (Glyphs, Option<FontRestore>, String) {
    match pref {
        Symbols::Braille => return (Glyphs::Braille, None, "braille (config)".into()),
        Symbols::Block => {
            let face = console_face().map(|(_, f)| f);
            let g = block_for(face.as_deref());
            return (g, None, format!("{g:?} (config)"));
        }
        Symbols::Auto => {}
    }
    // Windows Terminal, VS Code e outros pseudoconsoles: fazem fallback de fonte.
    if !crate::win::is_classic_console() {
        return (
            Glyphs::Braille,
            None,
            "braille (terminal with font fallback)".into(),
        );
    }
    let Some((info, face)) = console_face() else {
        return (
            Glyphs::Braille,
            None,
            "braille (console font unknown)".into(),
        );
    };
    if covers(&face, BRAILLE) != Some(false) {
        return (Glyphs::Braille, None, format!("braille ({face})"));
    }
    for cand in CANDIDATES {
        if covers(cand, BRAILLE) == Some(true)
            && let Some(restore) = switch_font(&info, cand)
        {
            return (
                Glyphs::Braille,
                Some(restore),
                format!("braille: console font {face} -> {cand}"),
            );
        }
    }
    let g = block_for(Some(&face));
    (
        g,
        None,
        format!("{g:?}: {face} has no braille and no braille font is installed"),
    )
}

/// Blocos que a fonte tem: oitavos ou, sem eles, meio bloco (WGL4).
fn block_for(face: Option<&str>) -> Glyphs {
    match face.map(|f| covers(f, EIGHTHS)) {
        Some(Some(false)) => Glyphs::Halves,
        _ => Glyphs::Eighths,
    }
}

fn console_out() -> Option<windows::Win32::Foundation::HANDLE> {
    use windows::Win32::System::Console::{GetStdHandle, STD_OUTPUT_HANDLE};
    // SAFETY: sem pré-condições.
    unsafe { GetStdHandle(STD_OUTPUT_HANDLE) }
        .ok()
        .filter(|h| !h.is_invalid())
}

/// Fonte atual do console (estrutura completa e nome).
fn console_face() -> Option<(windows::Win32::System::Console::CONSOLE_FONT_INFOEX, String)> {
    use windows::Win32::System::Console::{CONSOLE_FONT_INFOEX, GetCurrentConsoleFontEx};
    let out = console_out()?;
    let mut info = CONSOLE_FONT_INFOEX {
        cbSize: size_of::<CONSOLE_FONT_INFOEX>() as u32,
        ..Default::default()
    };
    // SAFETY: handle do console; estrutura local com `cbSize` preenchido.
    unsafe { GetCurrentConsoleFontEx(out, false, &raw mut info) }.ok()?;
    let face = utf16_z(&info.FaceName);
    (!face.is_empty()).then_some((info, face))
}

/// Troca a fonte do console mantendo a altura; confere relendo a fonte em uso.
fn switch_font(
    original: &windows::Win32::System::Console::CONSOLE_FONT_INFOEX,
    face: &str,
) -> Option<FontRestore> {
    use windows::Win32::System::Console::SetCurrentConsoleFontEx;
    let out = console_out()?;
    let mut info = *original;
    info.nFont = 0;
    // TrueType de largura fixa (FF_MODERN | TMPF_TRUETYPE); largura escolhida pela fonte.
    info.FontFamily = 0x36;
    info.FontWeight = 400;
    info.dwFontSize.X = 0;
    info.FaceName = [0; 32];
    for (dst, src) in info.FaceName.iter_mut().zip(face.encode_utf16().take(31)) {
        *dst = src;
    }
    // SAFETY: handle do console; estrutura local completa.
    unsafe { SetCurrentConsoleFontEx(out, false, &raw const info) }.ok()?;
    let restore = FontRestore {
        original: *original,
    };
    // Recusada em silêncio (fonte que o conhost não aceita): devolve a original já no Drop.
    let now = console_face()?.1;
    now.eq_ignore_ascii_case(face).then_some(restore)
}

fn utf16_z(buf: &[u16]) -> String {
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

/// A fonte `face` está instalada e tem todos os caracteres de `chars`. `None` = a fonte não
/// existe (a GDI trocaria por outra) ou a consulta falhou.
pub fn covers(face: &str, chars: &str) -> Option<bool> {
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateFontIndirectW, DEFAULT_CHARSET, DeleteDC, DeleteObject,
        GGI_MARK_NONEXISTING_GLYPHS, GetGlyphIndicesW, GetTextFaceW, LOGFONTW, SelectObject,
    };
    use windows::core::PCWSTR;
    let mut lf = LOGFONTW {
        lfHeight: -16,
        lfCharSet: DEFAULT_CHARSET,
        ..Default::default()
    };
    for (dst, src) in lf.lfFaceName.iter_mut().zip(face.encode_utf16().take(31)) {
        *dst = src;
    }
    // SAFETY: DC de memória e fonte criados e liberados aqui; buffers locais com os tamanhos
    // informados.
    unsafe {
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            return None;
        }
        let font = CreateFontIndirectW(&raw const lf);
        if font.is_invalid() {
            let _ = DeleteDC(dc);
            return None;
        }
        let old = SelectObject(dc, font.into());
        let mut name = [0u16; 64];
        let n = GetTextFaceW(dc, Some(&mut name));
        let real = utf16_z(&name[..(n.max(0) as usize).min(name.len())]);
        let result = if real.eq_ignore_ascii_case(face) {
            let text: Vec<u16> = chars.encode_utf16().collect();
            let mut idx = vec![0u16; text.len()];
            let r = GetGlyphIndicesW(
                dc,
                PCWSTR(text.as_ptr()),
                text.len() as i32,
                idx.as_mut_ptr(),
                GGI_MARK_NONEXISTING_GLYPHS,
            );
            (r != u32::MAX).then(|| idx.iter().all(|&g| g != 0xFFFF))
        } else {
            None
        };
        SelectObject(dc, old);
        let _ = DeleteObject(font.into());
        let _ = DeleteDC(dc);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// O que o modo meio bloco usa (todos do WGL4).
    const HALVES: &str = "▄▀█▌░_¯";

    #[test]
    fn nomes_da_opcao() {
        for s in [Symbols::Auto, Symbols::Braille, Symbols::Block] {
            assert_eq!(Symbols::from_name(s.name()), Some(s));
        }
        assert_eq!(Symbols::from_name("ascii"), None);
    }

    /// Fontes do Windows: a Consolas (padrão do conhost no Windows 10) não tem braille, mas tem
    /// os blocos; uma fonte inexistente não é confundida com a substituta da GDI.
    #[test]
    fn cobertura_das_fontes_do_sistema() {
        assert_eq!(covers("Fonte Que Nao Existe 123", BRAILLE), None);
        // Consolas e Lucida Console só têm os blocos do WGL4 (sem ▁▂▃▅▆▇▔): modo meio bloco.
        if let Some(b) = covers("Consolas", BRAILLE) {
            assert!(!b, "Consolas com braille?");
            assert_eq!(covers("Consolas", EIGHTHS), Some(false));
            assert_eq!(covers("Consolas", HALVES), Some(true));
            assert_eq!(block_for(Some("Consolas")), Glyphs::Halves);
        }
        if let Some(b) = covers("Cascadia Mono", BRAILLE) {
            assert!(b, "Cascadia Mono sem braille?");
        }
    }
}
